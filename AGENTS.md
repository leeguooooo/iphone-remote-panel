# Agent conventions for this repository

## 构建约束 (Build constraints)

重型 Rust build/test 必须通过 `heavy-cargo`(不要直接并发多个 `cargo test`/`cargo build`):

```bash
~/.local/bin/heavy-cargo test -p server
~/.local/bin/heavy-cargo build --release --bin iphone-use
```

- `heavy-cargo` 已做机器级限流(`HEAVY_SEM_JOBS`,默认 1 个重型 cargo 同时跑;32GB+ 内存可设 2)
  + 限制每个 cargo 的 rustc 并发(`CARGO_BUILD_JOBS=2`)
  + 启用 sccache 共享编译缓存(跨 worktree 复用第三方依赖)
  + 低优先级运行(`nice` + `taskpolicy -b`),不拖垮交互进程。
- 裸 `cargo` 仅用于轻量操作(`cargo check` 单文件、`cargo fmt`、`cargo clippy` 之类);
  它不带 sccache、不受并发闸约束——重型任务走裸 cargo 等于没缓存也没限流。
- 多个 worktree 并行时保持各自独立的 `target/` 目录(不要设全局 `CARGO_TARGET_DIR`);
  跨 worktree 的复用靠 sccache,不靠共享 target。
- 若 `~/.local/bin/heavy-cargo` 不存在(如 CI 或其他机器),回退裸 cargo 即可。

## Shared worktree discipline

Several agent sessions commit from this one checkout. Before every commit:
`git status` and `git diff --cached --stat`, stage **by file** (`git add <path>`), and never use `git add -A` / `git commit -a`. A file you did not edit that shows as modified belongs to another session — leave it. On 2026-09-05 a `git add` of `scripts/setup-wda.sh` swept another session's half-finished multi-instance rewrite into v0.6.1 and broke the release gate. Attribution: read the `Claude-Session:` trailer, not the author name (all sessions share it).
