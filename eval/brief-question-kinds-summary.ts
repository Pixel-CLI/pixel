// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

// Retain the non-secret arena evidence needed to reproduce the experiment table.
// Usage: bun eval/brief-question-kinds-summary.ts <results-directory> [...]
import { createHash } from "node:crypto";
import { basename, join } from "node:path";

type Row = {
  task: string; rep: string; arm: string; status: string;
  tokens: number | null; seconds: number | null;
};
type Event = { type: string; item?: { type: string; text?: string } };

const digest = (text: string) => createHash("sha256").update(text).digest("hex");
const median = (values: number[]) => {
  const ordered = values.toSorted((left, right) => left - right);
  const middle = Math.floor(ordered.length / 2);
  return ordered.length % 2 ? ordered[middle] : (ordered[middle - 1] + ordered[middle]) / 2;
};
const collect = async (directory: string) => {
  const rankingText = await Bun.file(join(directory, "ranking.json")).text();
  const ranking = JSON.parse(rankingText);
  const files: Record<string, string> = { "ranking.json": digest(rankingText) };
  const scenarios = [];
  for (const task of ranking.tasks) {
    const source = await Bun.file(join(ranking.run.scenarios_dir, `${task}.json`)).text();
    scenarios.push({ sha256: digest(source), scenario: JSON.parse(source) });
  }
  const answers = [];
  const receipts = [];
  for (let rep = 1; rep <= ranking.reps; rep += 1) {
    const name = `pixel-hook-${rep}.jsonl`;
    const file = Bun.file(join(directory, name));
    if (await file.exists()) {
      const text = await file.text();
      files[name] = digest(text);
      for (const line of text.trim().split("\n")) {
        const receipt = JSON.parse(line);
        receipts.push({
          rep,
          returncode: receipt.returncode,
          response_valid: receipt.response_valid,
          forwarded_to_codex: receipt.forwarded_to_codex,
          emitted_context: receipt.emitted_context,
          hook_event_name: receipt.hook_event_name,
          additional_context: receipt.additional_context,
          additional_context_bytes: receipt.additional_context_bytes,
          additional_context_sha256: receipt.additional_context_sha256,
        });
      }
    }
  }
  for (const row of ranking.rows) {
    const name = `${row.arm}-${row.task}-${row.rep}.jsonl`;
    const file = Bun.file(join(directory, name));
    if (await file.exists()) {
      const source = await file.text();
      files[name] = digest(source);
      const events = source.trim().split("\n").map((line): Event => JSON.parse(line));
      const messages = events.filter((event) => event.type === "item.completed" && event.item?.type === "agent_message");
      answers.push({ arm: row.arm, task: row.task, rep: row.rep, final_answer: messages.at(-1)?.item?.text ?? null });
    }
  }
  const paired = ranking.tasks.map((task: string) => {
    const rows: Row[] = ranking.rows.filter((row: Row) => row.task === task);
    const pairs = Array.from({ length: ranking.reps }, (_, index) => {
      const rep = String(index + 1);
      const raw = rows.find((row) => row.rep === rep && row.arm === "raw");
      const pixel = rows.find((row) => row.rep === rep && row.arm === "pixel");
      return { rep, raw, pixel };
    }).filter((pair): pair is { rep: string; raw: Row; pixel: Row } => pair.raw?.status === "complete" && pair.pixel?.status === "complete");
    const tokenPairs = pairs.filter(({ raw, pixel }) => raw.tokens !== null && raw.tokens > 0 && pixel.tokens !== null);
    const timed = pairs.filter(({ raw, pixel }) => raw.seconds !== null && pixel.seconds !== null);
    return {
      task,
      complete_pairs: pairs.length,
      token_pairs: tokenPairs.length,
      median_paired_token_saving_percent: tokenPairs.length
        ? median(tokenPairs.map(({ raw, pixel }) => 100 * (raw.tokens! - pixel.tokens!) / raw.tokens!)) : null,
      raw_median_seconds: timed.length ? median(timed.map(({ raw }) => raw.seconds!)) : null,
      pixel_median_seconds: timed.length ? median(timed.map(({ pixel }) => pixel.seconds!)) : null,
    };
  });
  return {
    results_directory_name: basename(directory),
    run: {
      run_id: ranking.run.run_id,
      model: ranking.run.model,
      effort: ranking.run.effort,
      codex_version: ranking.run.codex_version,
      pixel_image_id: ranking.run.pixel_image_id,
      pixel_source_id: ranking.run.pixel_source_id,
      skill_candidate_name: ranking.run.skill_candidate_name,
      skill_source_sha256: ranking.run.skill_source_sha256,
      fixture_repository: basename(ranking.run.repo_snapshot),
      fixture_commits_from_scenarios: [...new Set(scenarios.map(({ scenario }) => scenario.commit))],
      tasks: ranking.tasks,
      arms: ranking.arms,
      reps: ranking.reps,
    },
    scenario_provenance: "Scenario content and hashes read at export from the run's recorded scenario directory; scenario.commit identifies the fixture revision.",
    scenarios,
    rows: ranking.rows,
    answers,
    paired,
    receipts,
    sha256: files,
  };
};

if (Bun.argv.length < 3) throw new Error("Pass at least one arena results directory");
const runs = [];
for (const directory of Bun.argv.slice(2)) runs.push(await collect(directory));
console.log(JSON.stringify({
  schema_version: 1,
  token_definition: "input_tokens + generation_tokens; cached input is already included",
  time_definition: "Codex command wall seconds, including live hooks; excludes image/index/install/setup",
  quality_definition: "required-pattern score, not semantic adjudication",
  runs,
}, null, 2));
