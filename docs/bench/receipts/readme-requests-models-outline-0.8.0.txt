claim: readme-requests-models-outline
date: 2026-10-18T13:21:32Z
command: git clone https://github.com/psf/requests && cd requests && git checkout 611c616 && pixel list-signatures src/requests/models.py
exit: 0
--- pixel --version
pixel 0.8.0
commit: 9a8adca44865008d7a2d63bd6d8f0565a640ba27
target: aarch64-apple-darwin
rustc: rustc 1.99.0 (b940084d7 2026-09-28)
built: 2026-10-15
--- output
Cloning into 'requests'...
Note: switching to '611c616'.

You are in 'detached HEAD' state. You can look around, make experimental
changes and commit them, and you can discard any commits you make in this
state without impacting any branches by switching back to a branch.

If you want to create a new branch to retain commits you create, you may
do so (now or later) by using -c with the switch command. Example:

  git switch -c <new-branch-name>

Or undo this operation with:

  git switch -

Turn off this advice by setting config variable advice.detachedHead to false

HEAD is now at 611c6162 Bump the actions group with 3 updates (#7628)
pixel: built graph.db on first use (30 ms)
// src/requests/models.py [python]
  L  108  class  class RequestEncodingMixin:
  L  112  method  def path_url(self) -> str:
  L  151  method  def _encode_params(
  L  183  method  def _encode_files(
  L  254  class  class RequestHooksMixin:
  L  257  method  def register_hook(
  L  272  method  def deregister_hook(self, event: str, hook: _t.HookType) -> bool:
  L  284  class  class Request(RequestHooksMixin):
  L  323  method  def __init__(
  L  357  method  def __repr__(self) -> str:
  L  360  method  def prepare(self) -> PreparedRequest:
  L  378  class  class PreparedRequest(RequestEncodingMixin, RequestHooksMixin):
  L  407  method  def __init__(self) -> None:
  L  424  method  def prepare(
  L  453  method  def __repr__(self) -> str:
  L  456  method  def copy(self) -> PreparedRequest:
  L  467  method  def prepare_method(self, method: str | None) -> None:
  L  474  method  def _get_idna_encoded_host(host: str) -> str:
  L  483  method  def prepare_url(
  L  565  method  def prepare_headers(self, headers: Mapping[str, str | bytes] | None) -> None:
  L  576  method  def prepare_body(
  L  654  method  def prepare_content_length(self, body: _t.BodyType) -> None:
  L  670  method  def prepare_auth(
  L  699  method  def prepare_cookies(
  L  722  method  def prepare_hooks(self, hooks: _t.HooksInputType | None) -> None:
  L  732  class  class Response:
  L  765  method  def __init__(self) -> None:
  L  812  method  def __enter__(self) -> Self:
  L  815  method  def __exit__(self, *args: Any) -> None:
  L  818  method  def __getstate__(self) -> dict[str, Any]:
  L  826  method  def __setstate__(self, state: dict[str, Any]) -> None:
  L  834  method  def __repr__(self) -> str:
  L  837  method  def __bool__(self) -> bool:
  L  847  method  def __nonzero__(self) -> bool:
  L  857  method  def __iter__(self) -> Iterator[bytes]:
  L  862  method  def ok(self) -> bool:
  L  877  method  def is_redirect(self) -> bool:
  L  884  method  def is_permanent_redirect(self) -> bool:
  L  892  method  def next(self) -> PreparedRequest | None:
  L  897  method  def apparent_encoding(self) -> str | None:
  L  914  method  def iter_content(
  L  935  method  def generate() -> Generator[bytes, None, None]:
  L  994  method  def iter_lines(
  L 1035  method  def content(self) -> bytes:
  L 1054  method  def text(self) -> str:
  L 1091  method  def json(self, **kwargs: Any) -> Any:
  L 1127  method  def links(self) -> dict[str, dict[str, str]]:
  L 1144  method  def raise_for_status(self) -> None:
  L 1173  method  def close(self) -> None:

🟩 pixel list-signatures ❀ 120.3ms ❀ #b7c420
  │
  ├─ ⏱ no estimated time saving (baseline has no saved round trip)
  ├─ § full read 10365 tok, pixel answer 641 tok (-94%)
  │
  └────────────────────────────────────────────────────────
