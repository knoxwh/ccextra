# 官方 Claude 订阅透传实例(127.0.0.1:8223)

主实例(本仓库,8222)之外的第二个 ccextra 实例,只含一个 `anthropic_official` provider:把 Claude Code 自己订阅登录得到的 `Authorization` 与请求字节原样转给 `https://api.anthropic.com`。ccextra 不保存、不刷新任何订阅凭证。

本目录(`scripts/official/`)放该实例的模板与自检脚本;运行目录默认 `~/tools/ccextra-official`(可用 `CCEXTRA_OFFICIAL_DIR` 覆盖),是**纯运行目录**:删掉后由 `./service.sh official-init` 或开关 on 时的 `start` / `restart` 自动重建。

| 文件 | 说明 |
| --- | --- |
| `config.yaml.example` | 8223 配置模板:127.0.0.1:8223,无 `secret_key`、无 `payload`,provider `key` 为空,精确模型 + `claude-*` 通配,`alias == name`;`proxy_url` 生成时取主实例 `server.proxy_url` |
| `a1_check.sh` / `a1_mock.py` | A1 自检:本地 mock,只输出存在性与 UA 前缀,不打真实上游 |

运行目录内容(均由脚本生成):

| 文件 | 说明 |
| --- | --- |
| `ccextra` | 仓库根 `./ccextra` 的副本;`official-init` 在二进制不同时原子替换(不启停进程);开关 on 时 `start` / `deploy` 同步后对运行中的 8223 执行 kickstart 重启(plist 变化时先 bootout 再 bootstrap),`restart` 则是先停再同步并重新 bootstrap |
| `config.yaml` | 由模板生成,0600;已存在不覆盖(手工改动保留) |
| `ccextra.pid` | 启动后写入,停止时删除 |
| `logs/ccextra.launchd.log` | LaunchAgent `com.wanghao.ccextra-official` 的 stdout/stderr(bootstrap 时按日合并滚动并清理旧日日志) |

## 启停

开关在主实例 `config.yaml` 的 `official_instance: on|off` 一行(缺省 off):

```bash
cd ~/tools/ccextra
./service.sh status            # 两个实例状态 + 开关 + plist 是否安装
./service.sh official-init     # 只准备运行目录(不看开关、不启动)
./service.sh start|stop|restart
```

- `on`:`start` / `restart` 先 `official-init`,再安装 `~/Library/Application Support/ccextra/launchd/com.wanghao.ccextra-official.plist` 并拉起 8223(不在 LaunchAgents,登录不自启;崩溃 / 非零退出由 KeepAlive 重拉;旧版 `~/Library/LaunchAgents` 里的同名 plist 会被迁移删除;label 被禁用时报错提示 `launchctl enable`)。
- `off`:bootout 并删除该 plist,8223 不运行,重启电脑也不会起;运行目录保留。
- 改开关后执行 `./service.sh restart`(或 `start`)才生效;8222 进程本身不使用这个开关。

## 有 Claude 订阅账号后的启用步骤

1. **登录订阅**(交互式,在自己的终端里做):
   ```bash
   claude auth login        # 选 Claude 账号(claude.ai 订阅)登录
   claude auth status       # 确认 loggedIn: true
   ```
2. **打开开关**:编辑 `~/tools/ccextra/config.yaml`,把 `official_instance: off` 改成 `official_instance: on`(只改这一行)。
3. **启动**:
   ```bash
   cd ~/tools/ccextra && ./service.sh start && ./service.sh status
   curl -s http://127.0.0.1:8223/health     # 期望 ok
   ```
   `start` 不会打断已在跑的 8222;`restart` 会连 8222 一起重启。
4. **CC Switch 建/切 provider**:新建一个 Claude Code provider(如「Claude 官方订阅 8223」),env 只填
   - `ANTHROPIC_BASE_URL=http://127.0.0.1:8223`
   - **不要设** `ANTHROPIC_AUTH_TOKEN`、`ANTHROPIC_API_KEY`、`apiKeyHelper`(设了 CC 就不会发订阅登录的 Authorization)
   - 也不要沿用 8222 的 `ANTHROPIC_MODEL` / `ANTHROPIC_DEFAULT_*_MODEL` 别名(如带 `[1M]` 或 8222 专用名),留空用 CC 默认模型,或填官方模型 id(`claude-opus-5-5` / `claude-sonnet-5-5` / `claude-haiku-5-5`,其他 `claude-…` 快照名走通配)

   然后在 CC Switch 里切到这个 provider。
5. **A1 自检**(只打本地 mock,不消耗额度,只输出存在性):
   ```bash
   ~/tools/ccextra/scripts/official/a1_check.sh
   ```
   期望最后一行 `A1: PASS`(`/v1/messages` 带 `sk-ant-oat` 前缀的 Bearer `Authorization`、无 `x-api-key`)。FAIL 时先看 `claude auth status`,并确认当前 shell 及当前目录 `.claude/settings*.json` 没有注入 `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` / `apiKeyHelper`。
6. **确认登录方式**:新开 `claude`,输入 `/status`,确认 Login method 为 Claude 账号(claude.ai 订阅)、用量正常;再问一句并跑一次工具调用,在 `~/tools/ccextra-official/logs/ccextra.launchd.log` 里应看到 `access … provider=anthropic-official … status=200` 和 `official messages response … request_id=…`。

## 回退

1. CC Switch 切回原来的 8222 provider(恢复它原有的 `ANTHROPIC_BASE_URL` / token 设置)。
2. 把 `~/tools/ccextra/config.yaml` 的开关改回 `official_instance: off`。
3. `cd ~/tools/ccextra && ./service.sh start`(8222 不动,8223 停止并删除其 plist);`./service.sh status` 确认 `official: … health=down`、`official plist: not installed`。
4. 可选:`claude auth logout` 退出订阅登录;运行目录可直接删除,之后需要时再 `official-init` 重建。

## 注意

- 8223 只给本机 Claude Code 订阅用;不设 `secret_key`,只监听 127.0.0.1。
- 8222 设了 `secret_key`,任何 `sk-ant-` 开头的入口 key 在 8222 一律本地 401,不会转发。
- 二进制与 8222 同版本:`deploy.sh` 结束时调用 `service.sh start`,开关 on 时会同步二进制并重启 8223。
