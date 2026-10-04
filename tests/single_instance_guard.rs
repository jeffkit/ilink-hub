//! issue #28 验收测试 —— 第二个 Hub 实例必须 fail-fast 拒绝启动。
//!
//! 现状（未修复时）本文件 **RED**：两个 `ilink-hub serve` 进程共用同一 `DATABASE_URL`
//! （含 SQLite 文件型）都能启动成功 —— 各自持有进程内 `ClientState`/`InMemoryQueue`、
//! 各自轮询同一 bot token，"看起来像"支持水平扩展，实际会静默丢消息。
//!
//! 期望（修复后）本文件 **GREEN**：第二个实例 fail-fast 退出（非零退出码）并打印可操作
//! 指引（见 `GUIDANCE_MARKERS`）且指向文档（见 `DOC_POINTER_MARKERS`），第一个实例不受影响。
//!
//! 第二个用例 `second_instance_starts_after_first_releases_database` 覆盖相反路径：第一实例
//! 退出（SIGKILL，模拟崩溃）后锁必须由内核/DB 自动释放，新实例能正常启动。该用例修复前后
//! 都应 GREEN —— 它挡住「PID 文件 / 残留锁文件」实现：那会让崩溃后的 Hub 永久起不来，
//! 也会卡死桌面端的 `restart_hub`。
//!
//! 隔离性：每个子进程 `env_clear()` 后只注入 HOME（临时目录，承载
//! `~/.ilink-hub/relay-secret`、`Library/Application Support/ilink-hub/device_identity.json`）、
//! 独立 `DATABASE_URL`、固定 `ILINK_HUB_MASTER_KEY`，并关闭配对 relay —— 不触碰开发者真实
//! 的 `~/.ilink-hub`，不依赖网络。子进程输出写文件（不用管道）以避免管道写满阻塞子进程。

use std::fs::File;
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// 32 字节 key 的十六进制表示（与 `src/store/mod.rs` 测试用常量同源）。
const MASTER_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

/// 第二个实例被拒绝时输出里必须出现的指引关键词（任一命中即可）。
/// 覆盖中英文措辞，避免测试绑死某一句话；实现时若采用别的文案，需要同步这里。
const GUIDANCE_MARKERS: &[&str] = &[
    "another instance",
    "already running",
    "single instance",
    "single-instance",
    "另一个实例",
    "已在运行",
    "单实例",
];

/// 指引需指向文档（验收："stderr 打印指向本文档的指引"）。任一命中即可；
/// 覆盖 docker.md / faq.md / release-and-deploy.md 三种可能指向，实现方择一即可。
const DOC_POINTER_MARKERS: &[&str] = &[
    "docs/deployment/docker.md",
    "docs/guide/faq.md",
    "release-and-deploy.md",
    "docker.md",
    "faq.md",
];

/// 子进程存活/退出的兜底上限。
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const SECOND_INSTANCE_TIMEOUT: Duration = Duration::from_secs(20);

struct ChildGuard {
    child: Child,
    stdout_path: std::path::PathBuf,
    stderr_path: std::path::PathBuf,
}

impl ChildGuard {
    fn spawn(name: &str, port: u16, home: &Path, db_url: &str) -> Self {
        let bin = std::env::var("CARGO_BIN_EXE_ilink-hub")
            .unwrap_or_else(|_| "./target/debug/ilink-hub".to_string());
        // stdout / stderr 分开落文件：验收要求指引出现在 **stderr**，混流则无法断言。
        let stdout_path = home.join(format!("{name}.out.log"));
        let stderr_path = home.join(format!("{name}.err.log"));
        let stdout = File::create(&stdout_path).expect("create child stdout file");
        let stderr = File::create(&stderr_path).expect("create child stderr file");
        // 指向一个没人监听的 loopback 端口：上游调用立刻 connection refused，
        // 子进程不会向真实 `ilinkai.weixin.qq.com` 发任何请求。
        let dead_upstream = format!("http://127.0.0.1:{}", free_port());
        let child = Command::new(bin)
            .arg("serve")
            .arg("--addr")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--ilink-base-url")
            .arg(dead_upstream)
            .arg("--token")
            .arg("bot@im.bot:wip-issue28-secret")
            .env_clear()
            .env("HOME", home)
            .env("DATABASE_URL", db_url)
            .env("ILINK_HUB_MASTER_KEY", MASTER_KEY_HEX)
            .env("ILINKHUB_RELAY", "0")
            .env("RUST_LOG", "info")
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {name}: {e}"));
        Self {
            child,
            stdout_path,
            stderr_path,
        }
    }

    /// 未退出则返回 `None`；已退出则返回 (exit_code, 合并输出)。
    fn try_exit(&mut self) -> Option<(Option<i32>, String)> {
        self.child
            .try_wait()
            .expect("try_wait")
            .map(|status| (status.code(), self.output()))
    }

    /// stdout + stderr 合并，仅用于失败诊断。
    fn output(&self) -> String {
        format!(
            "--- stdout ---\n{}\n--- stderr ---\n{}",
            read_file(&self.stdout_path),
            read_file(&self.stderr_path)
        )
    }

    /// 仅 stderr —— 指引必须出现在这里。
    fn stderr(&self) -> String {
        read_file(&self.stderr_path)
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_file(path: &Path) -> String {
    let mut s = String::new();
    if let Ok(mut f) = File::open(path) {
        let _ = f.read_to_string(&mut s);
    }
    s
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = l.local_addr().expect("local_addr").port();
    drop(l);
    port
}

fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn sqlite_url(dir: &Path) -> String {
    format!("sqlite:{}", dir.join("ilink-hub.db").display())
}

/// 核心断言：同一 `DATABASE_URL` 下，第二个实例必须被拒绝（非零退出 + 打印指引），
/// 且第一个实例在此期间保持存活。
#[test]
fn second_hub_instance_on_same_database_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_url = sqlite_url(dir.path());

    let port_a = free_port();
    let port_b = free_port();
    let mut a = ChildGuard::spawn("A", port_a, dir.path(), &db_url);

    assert!(
        wait_for_port(port_a, READY_TIMEOUT),
        "第一实例未能监听 127.0.0.1:{port_a}（{:?} 内）；其输出：\n{}",
        READY_TIMEOUT,
        a.output()
    );

    let mut b = ChildGuard::spawn("B", port_b, dir.path(), &db_url);

    let deadline = Instant::now() + SECOND_INSTANCE_TIMEOUT;
    let mut outcome = None;
    while Instant::now() < deadline {
        if let Some(exited) = b.try_exit() {
            outcome = Some(exited);
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    let (code, output) = match outcome {
        Some(v) => v,
        None => {
            let b_alive = wait_for_port(port_b, Duration::from_secs(2));
            panic!(
                "第二实例（B，--addr 127.0.0.1:{port_b}，与 A 共用 DATABASE_URL {db_url}）在 \
                 {SECOND_INSTANCE_TIMEOUT:?} 内仍未退出（监听中={b_alive}）—— 当前没有单实例 \
                 守卫：第二实例会正常起来，各自持有进程内 registry/queue，静默对半丢消息。\
                 B 的输出：\n{}",
                b.output()
            );
        }
    };

    assert!(
        code != Some(0),
        "第二实例退出码必须非零（fail-fast 拒绝启动），实际 {code:?}；输出：\n{output}"
    );

    // 验收要求指引出现在 **stderr**（`eprintln!` / `anyhow::bail!` 经 `main` 落到 stderr）；
    // 仅 `tracing::error!`（默认写 stdout）不算数。
    let lowered = b.stderr().to_lowercase();
    assert!(
        GUIDANCE_MARKERS
            .iter()
            .any(|m| lowered.contains(&m.to_lowercase())),
        "第二实例的 stderr 必须包含可操作指引（任一 {GUIDANCE_MARKERS:?}），实际输出：\n{output}"
    );
    assert!(
        DOC_POINTER_MARKERS
            .iter()
            .any(|m| lowered.contains(&m.to_lowercase())),
        "第二实例的 stderr 必须指向文档（任一 {DOC_POINTER_MARKERS:?}），实际输出：\n{output}"
    );

    // A 必须仍然存活：守卫不能误杀/误伤先到者。
    assert!(
        a.try_exit().is_none(),
        "第一实例不应被第二实例的启动影响而退出；其输出：\n{}",
        a.output()
    );
    assert!(
        TcpStream::connect(("127.0.0.1", port_a)).is_ok(),
        "第一实例应仍能接受连接"
    );
}

/// 相反路径：第一实例**被 SIGKILL**（模拟崩溃，不走 Drop）后，锁必须自动释放，
/// 新实例能正常启动。修复前后都应 GREEN —— 挡住 PID 文件 / 残留锁文件方案。
#[test]
fn second_instance_starts_after_first_releases_database() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_url = sqlite_url(dir.path());

    let port_a = free_port();
    let mut a = ChildGuard::spawn("A", port_a, dir.path(), &db_url);
    assert!(
        wait_for_port(port_a, READY_TIMEOUT),
        "第一实例未能监听 127.0.0.1:{port_a}（{:?} 内）；其输出：\n{}",
        READY_TIMEOUT,
        a.output()
    );

    // SIGKILL：进程不执行任何清理代码，锁只能靠内核（flock）/ 数据库会话（advisory lock）释放。
    a.kill();

    let port_b = free_port();
    let mut b = ChildGuard::spawn("B", port_b, dir.path(), &db_url);
    assert!(
        wait_for_port(port_b, READY_TIMEOUT),
        "第一实例退出后，新实例必须能启动 —— 锁未被释放（残留 PID/锁文件？）；\
         其输出：\n{}",
        b.output()
    );
    assert!(
        b.try_exit().is_none(),
        "新实例不应启动后立即退出；其输出：\n{}",
        b.output()
    );
}
