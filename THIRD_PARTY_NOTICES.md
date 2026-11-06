# 第三方组件与许可

LLMLocalSearch 自身采用 AGPL-3.0-only。安装包的构建脚本会从 Cargo 已锁定的 Windows 依赖生成组件清单，并收集包中提供的许可文件；其中 egui 字体的许可文件也会一并收集。

- Apache Tika 3.3.3：Apache-2.0，安装包包含其 JAR 内的 `META-INF/LICENSE` 和 `META-INF/NOTICE`；解析器仅在本机运行。
- Eclipse Temurin JRE 21.0.13+9：GPL-2.0 with Classpath Exception，安装包保留 JRE 的 `legal` 目录和 `NOTICE`。
- SQLite：公共领域；随 `rusqlite` 的 bundled SQLite 构建。
- NSIS：安装器使用其发布版本编译，具体许可及说明由 NSIS 上游提供。

部分 Rust crate 在 crates.io 发布包中没有顶层许可文本。`third-party/licenses/` 保存与对应发布包源代码提交一致的上游许可文件：

- `egui-MIT.txt`：emilk/egui@49682f8baa058bf49e011035cfbd6e825f88a5ef，适用于该版本的 egui 家族 crate。
- `accesskit-MIT.txt`：AccessKit/accesskit@a55d3e1a18bb9ef0e4bccc9083fb13c3e0ad8969。
- `clipboard-win-BSL.txt`：DoumanAsh/clipboard-win@ed27008ae5d1f10cfbb266de039f0fb35fd05a56。
- `gl-generator-Apache.txt`：brendanzab/gl-rs@ea503e8d5fb6d73c6030e6191ce738cd3bf3433e。
- `khronos-api-Apache.txt`：brendanzab/gl-rs@f150967b1c44ae888e6676f93f639ebc82771bdc。
- `profiling-MIT.txt`：aclysma/profiling@8271551172eb6fa4cba47369aedd93790c623df9。

依赖升级后应重新核对版本、许可和这些文件的来源，再构建安装包。
