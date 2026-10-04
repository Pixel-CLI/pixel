#!/usr/bin/env python3
"""Model adapters for the decide bake-off (protocol: docs/bench/decide-bakeoff-protocol.md).

Every adapter exposes:  name, load(), decide(spec) -> {label: prob}.
spec = {id, set, family, qtype, text, context, labels, criteria, expected}
Mapping mirrors upstream jevbench adapters (pixel_local / local_openjev /
verdict_local / laya_local) so results stay comparable: instructions ride in
`context`, options carry their criterion text, noul maps to yes/no.

Third-party packages (torch, transformers, laya, gliformer and the upstream
engine checkouts) are imported only inside the `load()`/`decide()` of the ML
adapter that needs them, never at module level: `keyword-router`,
`remote-cli`, `run.py` and `score.py` run on the standard library alone, and
the ML rows need a venv that has those packages. This is the
scoped exemption from the scripts' standard-library rule recorded in
`.coderabbit.yaml`.
"""
from __future__ import annotations

import json
import math
import os
import subprocess
import sys
from pathlib import Path

THREADS = 4
HF = Path.home() / ".cache" / "huggingface" / "hub"
SNAPSHOTS = {
    "verdict": HF / "models--heman10x--rlcd-modernbert-151m" / "snapshots" / "8af2496eb63c7fa66d7d234e1f62629380030eb4",
    "laya": HF / "models--convaiinnovations--laya" / "snapshots" / "1c5edc17a7acd8701df6fc341c0d179f1c62c982",
    "gavel": HF / "models--chukfinley--gavel-base" / "snapshots" / "af65efe9283ad7045c60e393d0c7a179b8581b72",
    "gliformer": HF / "models--knowledgator--gliformer-large-v1" / "snapshots" / "d0a4e53d09cebe6bc963dd9be319d4279084bb2d",
    "openjev-deberta": HF / "models--com-kotobalabs--open-jev-deberta-v3-large" / "snapshots" / "19bf9a64815add579fbf6c907bef584d9277a8e4",
}
VERDICT_ENGINE = "/tmp/verdict-engine"  # Heman10x-NGU/Verdict-open-jev clone


def softmax(xs):
    m = max(xs)
    e = [math.exp(x - m) for x in xs]
    s = sum(e)
    return [v / s for v in e]


def rubric(spec):
    return {l: spec["criteria"].get(l, l) for l in spec["labels"]}


def qtype(spec):
    return spec.get("qtype", "choice")


def criterion_text(spec, label):
    return spec["criteria"].get(label) or label


class KeywordRouter:
    """Pixel's shipped plan router: the name-set classifier ported 1:1 from
    crates/pixel-graph/src/plan.rs (same port as bench-plan-routing.py's
    `baseline`). Hard 0/1 probabilities — it is a rules row, not a forecast."""
    name = "keyword-router"

    def load(self):
        return self

    @staticmethod
    def _labels_for(text):
        import re
        words = re.findall(r"[^\W_]+", text.lower())
        def has(word):
            return any(w == word or (w.endswith("s") and w[:-1] == word) for w in words)
        phrase = " " + " ".join(words) + " "
        out = set()
        if any(has(w) for w in ["clickable", "interactive", "button", "link", "navigation", "click"]):
            out.add("dead-interactive")
        if " dead code " in phrase or has("unused") or has("remove"):
            out.add("dead-code")
        if any(has(w) for w in ["refactor", "hotspot", "priority"]):
            out.add("hotspots")
        if any(has(w) for w in ["recent", "bug", "regression"]):
            out.add("recent-changes")
        return out or {"by-concept"}

    def decide(self, spec):
        label = spec["id"].rsplit("::", 1)[-1]
        fired = self._labels_for(spec["text"])
        return {"yes": 1.0 if label in fired else 0.0,
                "no": 0.0 if label in fired else 1.0}


class Gavel:
    """chukfinley/gavel-base: premise=state, hypothesis='<question> The answer is <criterion>.'"""
    name = "gavel"
    TEMPERATURE = 2.2677  # fitted temperature shipped in gavel_config.json

    def load(self):
        import torch
        from transformers import AutoModelForSequenceClassification, AutoTokenizer
        torch.set_num_threads(THREADS)
        self.tok = AutoTokenizer.from_pretrained(str(SNAPSHOTS["gavel"]))
        self.model = AutoModelForSequenceClassification.from_pretrained(
            str(SNAPSHOTS["gavel"])).train(False)
        return self

    def decide(self, spec):
        import torch
        premise = spec["text"] if not spec["context"] else f'{spec["context"]}\n\n{spec["text"]}'
        hyps = [f'The answer is {criterion_text(spec, l)}.' for l in spec["labels"]]
        batch = self.tok([premise] * len(hyps), hyps, padding=True, truncation=True,
                         max_length=512, return_tensors="pt")
        with torch.no_grad():
            logits = self.model(**batch).logits[:, 0] / self.TEMPERATURE
        probs = torch.softmax(logits, dim=-1).tolist()
        return dict(zip(spec["labels"], probs))


class Verdict:
    """heman10x/rlcd-modernbert-151m via the author's DecisionEngine (GLiClass one-pass)."""
    name = "verdict"
    ABSTAIN = "__insufficient_evidence__"

    def load(self):
        import torch
        torch.set_num_threads(THREADS)
        if VERDICT_ENGINE not in sys.path:
            sys.path.insert(0, VERDICT_ENGINE)
        from core.engine_encoder import DecisionEngine
        self.engine = DecisionEngine(model_name_or_path=str(SNAPSHOTS["verdict"]), device="cpu")
        return self

    def decide(self, spec):
        from core.primitives import Choice, Level, Noul, Option, Score
        qt = qtype(spec)
        crit = rubric(spec)
        if qt == "choice":
            q = Choice(id="d", question=spec["context"],
                       options=[Option(id=k, description=v or k) for k, v in crit.items()])
        elif qt == "score":
            q = Score(id="d", question=spec["context"],
                      levels=[Level(id=l, description=crit[l], value=float(i))
                              for i, l in enumerate(spec["labels"])])
        else:
            prop = spec["context"]
            if crit.get("yes") or crit.get("no"):
                prop += f' (true: {crit.get("yes", "yes")}; false: {crit.get("no", "no")})'
            q = Noul(id="d", proposition=prop, semantics="conditional_on_sufficient_evidence_v2")
        out = self.engine.evaluate(spec["text"], [q])
        probs = dict(out.results[0].probabilities)
        probs.pop(self.ABSTAIN, None)
        if qt == "noul":
            probs = {"yes": probs.get("true", 0.0), "no": probs.get("false", 0.0)}
        total = sum(probs.values())
        if total <= 0:
            raise RuntimeError("verdict: no substantive probability mass")
        return {l: probs.get(l, 0.0) / total for l in spec["labels"]}


class Laya:
    """convaiinnovations/laya: [MASK]-per-option scorer, Jev-shaped API."""
    name = "laya"

    def load(self):
        import torch
        torch.set_num_threads(THREADS)
        import laya
        self.agent = laya.load(str(SNAPSHOTS["laya"]))
        return self

    def decide(self, spec):
        qt = qtype(spec)
        crit = rubric(spec)
        if qt == "noul":
            criteria = {"true": crit.get("yes", "yes"), "false": crit.get("no", "no")}
        elif qt == "score":
            criteria = [crit[l] for l in spec["labels"]]
        else:
            criteria = crit
        out = self.agent.predict(spec["text"], {"decision": {
            "type": qt, "instructions": spec["context"], "criteria": criteria}})
        ans = out["answers"]["decision"]
        if qt == "noul":
            p = float(ans["noul"])
            return {"yes": p, "no": 1.0 - p}
        probs = {str(k): float(v) for k, v in ans["probabilities"].items()}
        return {l: probs.get(l, 0.0) for l in spec["labels"]}


class Gliformer:
    """knowledgator/gliformer-large-v1 (jeff's backbone): per-class scorer, softmaxed."""
    name = "gliformer"

    def load(self):
        import torch
        torch.set_num_threads(THREADS)
        from gliformer import GLiFormer
        self.model = GLiFormer.from_pretrained(str(SNAPSHOTS["gliformer"]), load_tokenizer=True)
        self.model = self.model.to("cpu").train(False)
        return self

    def decide(self, spec):
        classes = [criterion_text(spec, l) for l in spec["labels"]]
        text = spec["text"] if not spec["context"] else f'{spec["context"]}\n\n{spec["text"]}'
        preds = self.model.classify(text, classes, threshold=0.0)
        scores = {p.get("class_name", p.get("label")): float(p.get("score", 0.0)) for p in preds}
        raw = [max(scores.get(criterion_text(spec, l), 1e-9), 1e-9) for l in spec["labels"]]
        probs = softmax([math.log(s) for s in raw])
        return dict(zip(spec["labels"], probs))


class OpenJevDeberta:
    """com-kotobalabs/open-jev-deberta-v3-large: bundled typed_decisions module."""
    name = "openjev-deberta"

    def load(self):
        import torch
        torch.set_num_threads(THREADS)
        snap = str(SNAPSHOTS["openjev-deberta"])
        if snap not in sys.path:
            sys.path.insert(0, snap)
        from typed_decisions.open_jev import OpenJev
        self.model = OpenJev.from_pretrained(snap, device="cpu")
        return self

    def decide(self, spec):
        qt = qtype(spec)
        instructions = (spec["context"] + "\nAllowed answers and rubric: "
                        + json.dumps(rubric(spec), ensure_ascii=False))
        options = spec["labels"]
        if qt == "noul":
            options = ["no", "yes"]  # OpenJev noul readout: index 1 = yes
        answers = self.model.decide(spec["text"], [
            {"type": qt, "instructions": instructions, "options": options}])
        ans = answers[0]
        if qt == "noul":
            p = float(ans["noul"])
            return {"yes": p, "no": 1.0 - p}
        probs = ans["probabilities"]
        return {l: float(probs.get(l, 0.0)) for l in spec["labels"]}


class GteReranker:
    """Alibaba-NLP/gte-reranker-modernbert-base: pair (query, option-doc) logits -> softmax."""
    name = "gte-reranker"
    REPO = "Alibaba-NLP/gte-reranker-modernbert-base"

    def load(self):
        import torch
        from transformers import AutoModelForSequenceClassification, AutoTokenizer
        torch.set_num_threads(THREADS)
        self.tok = AutoTokenizer.from_pretrained(self.REPO)
        self.model = AutoModelForSequenceClassification.from_pretrained(self.REPO).train(False)
        return self

    def decide(self, spec):
        import torch
        query = spec["text"] if not spec["context"] else f'{spec["context"]}\n\n{spec["text"]}'
        docs = [criterion_text(spec, l) for l in spec["labels"]]
        batch = self.tok([(query, d) for d in docs], padding=True, truncation=True,
                         max_length=512, return_tensors="pt")
        with torch.no_grad():
            logits = self.model(**batch).logits[:, -1]
        probs = torch.softmax(logits.float(), dim=-1).tolist()
        return dict(zip(spec["labels"], probs))




class RemoteCli:
    """Remote decision: `pixel classify --jsonl`.

    One OpenAI-compatible chat completion per spec (OpenRouter / Ollama Cloud
    / local), opt-in and non-deterministic. The preset is passed as a flag so
    the same adapter serves every provider; the model is passed only when one
    is set (argument, then `REMOTE_MODEL`), otherwise `pixel` resolves it
    (`PIXEL_REMOTE_MODEL`, then the preset default). `resolved_model` records
    the model `pixel` reports in each reply's `snapshot.model`, so the run
    manifest names the model that actually answered.
    """
    name = "remote-cli"

    def __init__(self, preset="ollama", model=None, binary="pixel"):
        self.binary = binary
        self.preset = preset
        self.model = model or os.environ.get("REMOTE_MODEL") or None
        self.resolved_model = None
        self._proc = None

    def _cmd(self):
        cmd = [self.binary, "classify", "--jsonl",
               "--remote-preset", self.preset]
        if self.model:
            cmd += ["--remote-model", str(self.model)]
        return cmd

    def load(self):
        self._ensure()
        return self

    def _ensure(self):
        if self._proc is None or self._proc.poll() is not None:
            self._proc = subprocess.Popen(
                self._cmd(), stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL, text=True, bufsize=1)

    def decide(self, spec):
        self._ensure()
        body = {"text": spec["text"], "context": spec["context"],
                "labels": spec["labels"], "criteria": rubric(spec)}
        self._proc.stdin.write(json.dumps(body) + "\n")
        self._proc.stdin.flush()
        doc = json.loads(self._proc.stdout.readline())
        if not doc.get("ok"):
            raise RuntimeError(f"pixel classify remote: {doc.get('error')}")
        self.resolved_model = (doc.get("snapshot") or {}).get("model", self.resolved_model)
        return {l: float(doc["probs"][l]) for l in spec["labels"]}


ADAPTERS = {
    "keyword-router": KeywordRouter,
    "gavel": Gavel,
    "verdict": Verdict,
    "laya": Laya,
    "gliformer": Gliformer,
    "openjev-deberta": OpenJevDeberta,
    "gte-reranker": GteReranker,
    "remote-cli": RemoteCli,
}
