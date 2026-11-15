# barwatch-host

The window host of the barwatch desktop app: a copy of
[`damascene-winit-wgpu`](https://crates.io/crates/damascene-winit-wgpu) 0.9.0
(upstream commit `278ad7c9` of <https://github.com/computer-whisperer/damascene>,
`crates/damascene-winit-wgpu`), by Christian Balcom, licensed MIT OR Apache-2.0.
The licence texts it came with are `LICENSE-MIT` and `LICENSE-APACHE` in this
folder, unchanged.

It is a copy, not a dependency, because damascene's integration crates are
quick-start hosts: when an app needs behaviour they do not wire up, it copies
one and changes it (`DESIGN.md` §2 and §5.5). `damascene-core`, `damascene-wgpu`
and `damascene-winit` stay unmodified crates.io dependencies.

## What differs from upstream

- **`WinitWgpuApp::pointer_moved(position, cx) -> bool`** (new, default no-op).
  The host calls it on every `CursorMoved` with the pointer's position in
  logical pixels, and with `None` on `CursorLeft`; when it returns `true` the
  host draws a frame. Upstream keeps the position to itself and tells an app
  only when the hovered element changes, which is not enough for a hover line
  that follows the pointer inside one element. An app opts in by implementing
  `WinitWgpuApp` and starting through `run_host_app` /
  `run_host_app_with_config`; `run` and `run_with_config` take a plain `App`
  and never call the hook.
- The package is named `barwatch-host` (library `barwatch_host`), is not
  published, and takes its version from the workspace.
- For the crate's own tests only: the gradient shader they load is copied in as
  `src/test_gradient.wgsl` (from `damascene-core` 0.9.0, same licence), and one
  doc example names this crate.

Everything else is upstream's source as published, not reformatted with this
repository's `rustfmt.toml`: keep it that way so a later upstream release can
be diffed against it. Changed places carry a `barwatch:` comment.

## Taking a newer upstream release

Diff the new release against 0.9.0, apply that diff here, and keep the
`barwatch:` places in `src/lib.rs`.
