修复 misc 模块修复：S-03, D-01, D-02

## 待修复条目

  - [S-03] {{MESSAGE}} 注入风险缺少 README 静态警告
     文件：README.md, docs/bridge-config.md
     问题：
     修复方向：在 `{{MESSAGE}}` 配置示例旁添加安全警告框，说明不要用于 shell `-c` 参数，推荐 `stdin: message` 模式。

  - [D-01] sqlx 三驱动同时编译，二进制体积和攻击面增大
     文件：Cargo.toml:70
     问题：
     修复方向：引入 `[features]` 并设 `default = ["sqlite"]`，postgres/mysql 作为可选特性。注意这是 **breaking change**，需配合文档更新。

  - [D-02] rand 版本落后（0.8 → 0.9）
     文件：Cargo.toml:64
     问题：
     修复方向：升级 `rand = "0.9"`；`rand::thread_rng().gen::<u32>()` → `rand::random::<u32>()`；检查 `ed25519-dalek` 的 `rand_core` 兼容性。

## 完成标准
- [ ] S-03 修复已提交，相关测试通过
- [ ] D-01 修复已提交，相关测试通过
- [ ] D-02 修复已提交，相关测试通过
- [ ] cargo clippy 无新 warning
- [ ] cargo test 全绿

## 非目标
- 不重构不涉及上述条目的其他模块
- 不升级无关依赖