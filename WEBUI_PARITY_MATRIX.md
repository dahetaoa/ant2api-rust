# WebUI 奇偶校验矩阵 (Go → Rust)

本文档定义了 Go WebUI 需要在 Rust 中实现的所有路由、行为和消息，作为验收标准。

## 一、路由矩阵

### 1. 公开路由（无需认证）

| 路径 | 方法 | 描述 | 输出格式 | 关键响应头 | 状态码 | 关键中文消息 |
|------|------|------|----------|-----------|-------|-------------|
| `/login` | GET | 显示登录页面（已登录时重定向到"/"） | HTML（完整页面） | - | 200/302 | - |
| `/login` | POST | 处理登录表单 | HTML或"登录成功" | HX-Redirect:"/" | 200 | "管理密码未配置，请设置 WEBUI_PASSWORD 环境变量" / "密码错误" / "登录成功" |
| `/logout` | GET | 登出，清除cookie并重定向 | - | Set-Cookie（清除） | 302→/login | - |
| `/health` | GET/HEAD | 健康检查（保持现有实现） | text/plain "ok" | - | 200 | - |

### 2. 受保护的管理界面路由

| 路径 | 方法 | 描述 | 输出格式 | 关键响应头 | 状态码 | 关键中文消息 |
|------|------|------|----------|-----------|-------|-------------|
| `/` | GET | Dashboard 主页面 | HTML（完整页面） | - | 200 | - |
| `/*` (catch-all) | GET | 未知路径返回 Dashboard（如 `/oauth-callback`） | HTML（完整页面） | - | 200 | - |

### 3. 受保护的 Manager API 路由

| 路径 | 方法 | 描述 | 输出格式 | 关键响应头 | 状态码/错误 | 关键中文消息 |
|------|------|------|----------|-----------|-------------|-------------|
| `/manager/api/stats` | GET | 获取统计卡片 | HTML片段 | - | 200 | - |
| `/manager/api/list` | GET | 获取账号列表 | HTML片段 | HX-Trigger: "refreshQuota" | 200 | "暂无数据" |
| `/manager/api/delete` | POST | 删除账号 | 空body或404 | - | 200/404 | "未找到" |
| `/manager/api/toggle` | POST | 切换启用/禁用 | HTML片段（TokenCard） | HX-Trigger: "refreshQuota" | 200 | - |
| `/manager/api/refresh` | POST | 刷新单个账号 | HTML片段（TokenCard） | HX-Trigger: "refreshQuota" | 200 | - |
| `/manager/api/refresh_all` | POST | 刷新所有账号 | 空body | HX-Trigger: "refreshStats, refreshList" | 200 | - |
| `/manager/api/quota` | GET/HEAD | 获取单个账号配额 | HTMX:HTML片段 / 否则:JSON | - | 200 | "缺少 id 参数" / "未找到对应账号" / 配额错误消息 |
| `/manager/api/quota/all` | POST | 获取所有账号配额 | HTMX:OOB HTML片段 / 否则:JSON | - | 200 | 配额错误消息 |
| `/manager/api/oauth/url` | GET | 生成OAuth授权URL | JSON: `{url: "..."}` / `{error: "..."}` | - | 200/500 | "生成 OAuth state 失败" |
| `/manager/api/oauth/parse-url` | POST | 解析回调URL并添加账号 | JSON: `{success: true}` / `{error: "..."}` | - | 200/400/500 | 见OAuth错误消息部分 |
| `/manager/api/settings` | GET/HEAD | 获取设置 | HTMX:HTML片段 / 否则:JSON | - | 200 | - |
| `/manager/api/settings` | POST | 保存设置 | JSON: `{success: true}` / `{error: "..."}` | - | 200/400/500 | "WebUI 登录密码不能为空" / "日志级别必须是 off、low 或 high" |

## 二、认证机制

### Cookie 规范
- **名称**: `grok_admin_session`
- **值**: `authenticated`
- **属性**: HttpOnly=true, Path="/", Expires=now+24h

### 未认证处理
- `/manager/api/*` 路径: 返回 401，body="未登录或会话已过期，请先登录管理面板"
- 其他受保护路径: 302 重定向到 `/login`

## 三、模板/HTML 结构 (pixel parity)

### CDN 依赖（必须保持完全一致）
```html
<script src="https://unpkg.com/htmx.org@1.9.10"></script>
<script src="https://cdn.tailwindcss.com"></script>
<link href="https://fonts.googleapis.com/css2?family=Inter:wght@300;400;500;600;700&display=swap" rel="stylesheet"/>
```

### HTMX 触发器名称（必须完全一致）
- `refreshStats` - 刷新统计卡片
- `refreshList` - 刷新账号列表
- `refreshQuota` - 刷新所有配额面板
- `settingsTabActivated` - 激活设置标签页

### Toast 系统
- 事件名: `showMessage`
- 详情: `{message: string, type: 'info'|'success'|'error'}`

### 关键 DOM ID
- `#toast-container` - Toast 容器
- `#tab-accounts` - 账号管理标签页
- `#tab-settings` - 系统设置标签页
- `#tokenGrid` - 账号卡片网格
- `#quota-{sessionId}` - 配额面板容器
- `#settings-container` - 设置表单容器

## 四、配额相关

### 缓存语义
- 成功缓存 TTL: 2分钟
- 错误缓存 TTL: 30秒
- 请求超时: 20秒
- 最大并发数: 4
- 按 sessionId 去重 inflight 请求

### 配额分组键
- "Claude/GPT" - claude-*, gpt-*
- "Gemini 3 Pro" - gemini-3-pro-high*
- "Gemini 3 Flash" - gemini-3-flash*
- "Gemini 3 Pro Image" - gemini-3-pro-image*
- "Gemini 2.5 Pro/Flash/Lite" - 其他

### 配额错误消息映射
| 条件 | 中文消息 |
|------|---------|
| status=401 | "Token 已失效或无权限，无法获取配额" |
| status=429 | "请求过于频繁，请稍后重试" |
| context timeout | "请求超时，无法获取配额" |
| context canceled | "请求已取消" |
| 其他 | "无法获取配额：{message}" |

## 五、OAuth 流程

### 授权URL生成
- redirect_uri: `http://localhost:{port}/oauth-callback`
- state TTL: 10分钟
- 一次性验证（使用后立即删除）

### 回调URL解析容错（MUST FIX）
Go 的 url.Parse 接受以下格式：
- 完整URL: `http://localhost:8045/oauth-callback?code=xxx&state=yyy`
- 无协议: `localhost:8045/oauth-callback?code=xxx&state=yyy`
- 仅路径: `/oauth-callback?code=xxx&state=yyy`

**Rust 必须实现相同的容错解析！**

### OAuth 错误消息
| 场景 | 中文消息 |
|------|---------|
| URL为空 | "请粘贴回调 URL" |
| 解析失败/无code | "回调 URL 中缺少 code 参数" |
| 无state | "回调 URL 中缺少 state 参数" |
| state验证失败 | "state 校验失败或已过期，请重新发起 OAuth 授权" |
| token交换失败 | "交换 Token 失败：请确认授权码未过期，且 redirect_uri 与发起授权时一致" |
| 无法获取projectId且不允许随机 | "无法自动获取 Google 项目 ID，可能会导致部分接口 403。请填写自定义项目ID，或勾选"允许使用随机项目ID"。" |
| 保存失败 | "保存账号失败" |

## 六、设置系统

### WebUISettings 结构
```json
{
  "apiKey": string,
  "webuiPassword": string,
  "debug": "off"|"low"|"high",
  "userAgent": string,
  "gemini3MediaResolution": ""|"low"|"medium"|"high"
}
```

### .env 文件操作
- 搜索路径：从当前目录向上，遇到 Cargo.toml/.git 停止
- 不存在时：在当前目录创建
- 更新逻辑：保留无关行，更新已有键，追加新键
- 引号规则：包含空格/引号/为空 → `KEY="value"`

### 立即生效要求 (Rust 特有)
保存设置后，以下配置必须立即生效（无需重启）：
1. `webui_password` - 登录验证
2. `api_user_agent` - Vertex/OAuth 请求 User-Agent
3. `gemini3_media_resolution` - OpenAI 转换时的媒体分辨率
4. `debug` - 日志级别

**注意：API_KEY 鉴权先不动，仅持久化和显示，不影响当前请求验证逻辑。**

## 七、实现约束

### 复用现有 Rust 模块（禁止重复实现）
- OAuth: `rust/src/credential/oauth.rs` - 仅扩展 parse_oauth_url
- Store: `rust/src/credential/store.rs` - 直接使用
- Vertex: `rust/src/vertex/client.rs` - 使用 fetch_available_models
- Model: `rust/src/util/model.rs` - 使用 canonical_model_id

### 模板位置
- 唯一位置: `rust/templates/`
- 不要在 `rust/src/gateway/manager/` 下创建模板副本

### 不要修改的部分
- `/v1/*` 路由的行为
- API_KEY 鉴权逻辑
- 现有的流式/非流式转换逻辑

## 八、验收检查清单

### Pixel Parity
- [x] /login 页面布局/字体/间距与 Go 一致
- [x] / dashboard 标签页/卡片/配额面板与 Go 一致
- [x] 设置页面表单/按钮样式与 Go 一致

### 行为 Parity
- [x] OAuth 流程：复制地址栏 URL 后可正常解析
- [x] 无协议/仅路径 URL 可正常解析
- [x] HTMX 触发器正常工作
- [x] quota/all OOB 交换正常更新各面板
- [x] 设置标签页懒加载
- [x] 重置按钮获取 JSON
- [x] 保存按钮显示 toast
- [x] 设置立即生效

### 边界情况
- [x] WEBUI_PASSWORD 为空时显示正确错误
- [x] 无账号时显示"暂无数据"
- [x] 配额获取失败显示正确中文消息
- [x] 并发配额请求遵守去重和并发限制

## 九、实现状态 (2026-01-26)

### 已完成模块
1. **runtime_config.rs** - 运行时配置热更新系统
2. **gateway/manager/mod.rs** - Manager 模块入口
3. **gateway/manager/handler.rs** - 所有 WebUI 路由处理器
4. **gateway/manager/quota.rs** - 配额缓存与获取逻辑
5. **gateway/manager/templates.rs** - Askama 模板结构与辅助函数

### 模板文件
- `templates/base.html` - 基础布局
- `templates/login.html` - 登录页面
- `templates/dashboard.html` - Dashboard 主页面
- `templates/fragments/stats_cards.html` - 统计卡片
- `templates/fragments/token_list.html` - 账号列表
- `templates/fragments/token_card.html` - 单个账号卡片
- `templates/fragments/quota_skeleton.html` - 配额骨架屏
- `templates/fragments/quota_content.html` - 配额内容
- `templates/fragments/quota_swap_oob.html` - 配额 OOB 交换
- `templates/fragments/settings.html` - 设置页面

### 路由实现
| 路由 | 状态 |
|------|------|
| GET /login | ✅ |
| POST /login | ✅ |
| GET /logout | ✅ |
| GET / | ✅ |
| GET /manager/api/stats | ✅ |
| GET /manager/api/list | ✅ |
| POST /manager/api/delete | ✅ |
| POST /manager/api/toggle | ✅ |
| POST /manager/api/refresh | ✅ |
| POST /manager/api/refresh_all | ✅ |
| GET /manager/api/quota | ✅ |
| POST /manager/api/quota/all | ✅ |
| GET /manager/api/oauth/url | ✅ |
| POST /manager/api/oauth/parse-url | ✅ |
| GET /manager/api/settings | ✅ |
| POST /manager/api/settings | ✅ |

