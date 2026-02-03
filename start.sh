#!/bin/bash
#
# Antigravity 2 API (Rust) - 启动脚本
# 功能：加载 .env、检查必要环境变量、按需构建并启动服务
#

set -e

# 颜色定义
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

# 配置
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SERVER_BIN="$SCRIPT_DIR/server"
BUILD_HASH_FILE="$SCRIPT_DIR/.build_hash"

# 清屏
clear

echo -e "${CYAN}╔══════════════════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║       ${GREEN}Antigravity 2 API${CYAN} - Rust 启动脚本                  ║${NC}"
echo -e "${CYAN}╚══════════════════════════════════════════════════════════╝${NC}"
echo ""

# =============================================================================
# 函数定义
# =============================================================================

log_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[✓]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[!]${NC} $1"
}

log_error() {
    echo -e "${RED}[✗]${NC} $1"
}

# 交互式清屏模式资源（仅在 start_server_with_clear 中使用）
CLEAR_LOG_FILE=""
CLEAR_SERVER_PID=""
CLEAR_TAIL_PID=""

cleanup_clear_mode() {
    if [[ -n "$CLEAR_TAIL_PID" ]]; then
        kill "$CLEAR_TAIL_PID" 2>/dev/null || true
        wait "$CLEAR_TAIL_PID" 2>/dev/null || true
        CLEAR_TAIL_PID=""
    fi
    if [[ -n "$CLEAR_SERVER_PID" ]]; then
        kill "$CLEAR_SERVER_PID" 2>/dev/null || true
        wait "$CLEAR_SERVER_PID" 2>/dev/null || true
        CLEAR_SERVER_PID=""
    fi
    if [[ -n "$CLEAR_LOG_FILE" && -f "$CLEAR_LOG_FILE" ]]; then
        rm -f "$CLEAR_LOG_FILE" || true
        CLEAR_LOG_FILE=""
    fi
}

# 检测端口是否被占用（监听中）。
port_is_listening() {
    local port="$1"
    if command -v ss &> /dev/null; then
        ss -ltnH "sport = :$port" 2>/dev/null | grep -q .
        return $?
    fi
    if command -v lsof &> /dev/null; then
        lsof -nP -iTCP:"$port" -sTCP:LISTEN &> /dev/null
        return $?
    fi
    return 1
}

# 尽量找出占用端口的 Docker 容器（如果有）。
find_docker_containers_by_port() {
    local port="$1"
    if ! command -v docker &> /dev/null; then
        return 0
    fi
    docker ps --format '{{.Names}}\t{{.Ports}}' 2>/dev/null | awk -v p=":$port->" -F'\t' '$2 ~ p {print $1}'
}

# 确保端口可用：若被占用则给出可操作的提示；若为 Docker 占用可选择停止容器。
ensure_port_available() {
    local port="$1"

    if ! port_is_listening "$port"; then
        return 0
    fi

    echo ""
    log_warn "端口 $port 已被占用，无法启动本地服务"

    # 优先判断是否为 docker-compose / docker 容器占用
    local containers
    containers="$(find_docker_containers_by_port "$port" | tr '\n' ' ' | xargs echo -n 2>/dev/null || true)"

    if [[ -n "$containers" ]]; then
        log_warn "检测到以下 Docker 容器占用该端口：$containers"
        echo -e "  你可以：\n  1) 停止容器后启动本地服务（推荐）\n  2) 修改 PORT 环境变量（例如 PORT=8046）"
        echo ""

        # 仅在交互式终端下询问（避免脚本在 CI/后台卡住）
        if [[ -t 0 ]]; then
            read -p "是否现在停止这些容器并继续启动？(y/N): " stop_choice
            if [[ "$stop_choice" =~ ^[Yy]$ ]]; then
                docker stop $containers >/dev/null 2>&1 || true
                log_success "已停止容器：$containers"
                return 0
            fi
        fi

        log_error "端口被 Docker 容器占用，已取消启动。请先 docker stop $containers 或设置 PORT 后重试。"
        exit 1
    fi

    # 非 docker 场景：打印占用信息（最佳努力）
    if command -v ss &> /dev/null; then
        ss -ltnp "sport = :$port" 2>/dev/null || true
    fi

    log_error "端口被其他进程占用。请释放端口或设置 PORT=其他端口后重试。"
    exit 1
}

# 加载 .env 文件
load_env() {
    if [[ -f "$SCRIPT_DIR/.env" ]]; then
        set -a
        # shellcheck disable=SC1090
        source "$SCRIPT_DIR/.env"
        set +a
        log_success "已加载 .env 配置文件"
    else
        log_warn ".env 文件不存在，将使用默认配置或环境变量"
        if [[ -f "$SCRIPT_DIR/.env.example" ]]; then
            log_info "可参考 $SCRIPT_DIR/.env.example"
        fi
    fi
}

# 检查必要的环境变量
check_required_vars() {
    local missing_vars=()
    local var_descriptions=(
        "WEBUI_PASSWORD:管理面板登录密码（必填）"
    )

    # 定义推荐配置的变量（非必须但推荐设置）
    local recommended_vars=(
        "API_KEY:API 访问密钥，用于保护 API 端点（可选，建议设置）"
    )

    echo -e "\n${CYAN}━━━ 环境变量检查 ━━━${NC}\n"

    # 检查必须的变量
    for item in "${var_descriptions[@]}"; do
        local var_name="${item%%:*}"
        local var_desc="${item#*:}"

        if [[ -z "${!var_name}" ]]; then
            missing_vars+=("$item")
        else
            log_success "$var_name 已设置"
        fi
    done

    # 提示推荐变量
    local unset_recommended=()
    for item in "${recommended_vars[@]}"; do
        local var_name="${item%%:*}"
        local var_desc="${item#*:}"

        if [[ -z "${!var_name}" ]]; then
            unset_recommended+=("$item")
        else
            log_success "$var_name 已设置"
        fi
    done

    # 如果有必须变量未设置
    if [[ ${#missing_vars[@]} -gt 0 ]]; then
        echo ""
        log_warn "以下必须的环境变量尚未设置："
        echo ""
        for item in "${missing_vars[@]}"; do
            local var_name="${item%%:*}"
            local var_desc="${item#*:}"
            echo -e "  ${YELLOW}$var_name${NC}"
            echo -e "    └─ $var_desc"
        done
        echo ""

        read -p "是否现在设置这些变量？(y/N): " setup_choice
        if [[ "$setup_choice" =~ ^[Yy]$ ]]; then
            setup_missing_vars "${missing_vars[@]}"
        else
            log_error "必须的变量未设置，无法启动"
            exit 1
        fi
    fi

    # 提示推荐变量
    if [[ ${#unset_recommended[@]} -gt 0 ]]; then
        echo ""
        log_warn "以下推荐的环境变量尚未设置（可选）："
        echo ""
        for item in "${unset_recommended[@]}"; do
            local var_name="${item%%:*}"
            local var_desc="${item#*:}"
            echo -e "  ${YELLOW}$var_name${NC}"
            echo -e "    └─ $var_desc"
        done
        echo ""

        read -p "是否现在设置这些变量？(y/N): " setup_choice
        if [[ "$setup_choice" =~ ^[Yy]$ ]]; then
            setup_missing_vars "${unset_recommended[@]}"
        fi
    fi
}

# 设置缺失的变量
setup_missing_vars() {
    local vars=("$@")
    local env_updates=""

    for item in "${vars[@]}"; do
        local var_name="${item%%:*}"
        local var_desc="${item#*:}"

        echo ""
        echo -e "${CYAN}设置 $var_name${NC}"
        echo -e "  说明: $var_desc"

        case "$var_name" in
            WEBUI_PASSWORD)
                echo -e "  ${YELLOW}提示: 建议使用强密码，至少8个字符${NC}"
                read -rsp "  请输入 $var_name: " var_value
                echo ""
                ;;
            API_KEY)
                echo -e "  ${YELLOW}提示: 格式如 sk-xxxxx，留空则禁用 API 密钥验证${NC}"
                read -rp "  请输入 $var_name (可留空): " var_value
                ;;
            *)
                read -rp "  请输入 $var_name: " var_value
                ;;
        esac

        if [[ -n "$var_value" ]]; then
            export "$var_name=$var_value"
            env_updates+="$var_name=$var_value\n"
            log_success "$var_name 已设置"
        else
            log_warn "$var_name 保持为空"
        fi
    done

    if [[ -n "$env_updates" ]]; then
        echo ""
        read -p "是否将这些设置保存到 .env 文件？(Y/n): " save_choice
        if [[ ! "$save_choice" =~ ^[Nn]$ ]]; then
            echo -e "$env_updates" >> "$SCRIPT_DIR/.env"
            log_success "已保存到 .env 文件"
        fi
    fi
}

# 计算源代码哈希
calculate_source_hash() {
    (
        find "$SCRIPT_DIR/src" -type f -name "*.rs" -print0 2>/dev/null || true
        find "$SCRIPT_DIR/templates" -type f -print0 2>/dev/null || true
        printf '%s\0' "$SCRIPT_DIR/Cargo.toml" "$SCRIPT_DIR/Cargo.lock"
    ) | xargs -0 -r md5sum 2>/dev/null | sort | md5sum | cut -d' ' -f1
}

# 获取上次构建的哈希
get_last_build_hash() {
    if [[ -f "$BUILD_HASH_FILE" ]]; then
        cat "$BUILD_HASH_FILE"
    else
        echo ""
    fi
}

# 保存构建哈希
save_build_hash() {
    echo "$1" > "$BUILD_HASH_FILE"
}

# 检查是否需要重新构建
needs_rebuild() {
    local current_hash
    current_hash=$(calculate_source_hash)
    local last_hash
    last_hash=$(get_last_build_hash)

    if [[ ! -f "$SERVER_BIN" ]]; then
        return 0
    fi
    if [[ "$current_hash" != "$last_hash" ]]; then
        return 0
    fi
    return 1
}

# 构建后端
build_backend() {
    log_info "正在构建后端（cargo build --release）..."
    cd "$SCRIPT_DIR"

    if ! command -v cargo &> /dev/null; then
        log_error "未找到 cargo，请先安装 Rust 工具链（rustup）。"
        exit 1
    fi

    cargo build --release --locked 2>&1
    cp -f "$SCRIPT_DIR/target/release/ant2api" "$SERVER_BIN"
    log_success "后端构建完成"
}

# 构建项目
build_project() {
    echo -e "\n${CYAN}━━━ 构建项目 ━━━${NC}\n"

    build_backend

    local current_hash
    current_hash=$(calculate_source_hash)
    save_build_hash "$current_hash"

    log_success "项目构建完成"
}

# 停止已运行的服务（排除 docker 容器内的进程）
stop_existing_server() {
    local pids
    pids=$(pgrep -f "$SERVER_BIN" 2>/dev/null || true)

    for pid in $pids; do
        if [[ -f "/proc/$pid/cgroup" ]] && grep -q docker "/proc/$pid/cgroup" 2>/dev/null; then
            continue
        fi
        log_info "正在停止已运行的服务 (PID: $pid)..."
        kill "$pid" 2>/dev/null || true
    done

    for pid in $pids; do
        if [[ -f "/proc/$pid/cgroup" ]] && grep -q docker "/proc/$pid/cgroup" 2>/dev/null; then
            continue
        fi
        while kill -0 "$pid" 2>/dev/null; do sleep 0.1; done
    done
}

# 启动服务
start_server() {
    echo -e "\n${CYAN}━━━ 启动服务 ━━━${NC}\n"

    stop_existing_server

    local port="${PORT:-8045}"
    local host="${HOST:-0.0.0.0}"

    ensure_port_available "$port"

    log_info "启动服务器（监听）: http://$host:$port"
    # 0.0.0.0 仅表示监听所有网卡，并不是可用于访问的目标地址
    log_info "本机访问建议: http://127.0.0.1:$port"
    log_info "健康检查: http://127.0.0.1:$port/health"
    echo ""

    cd "$SCRIPT_DIR"
    # 启用 jemalloc heap profiling：
    #   prof:true       - 启用堆分析
    #   lg_prof_sample:19 - 采样间隔 ~512KB (2^19 bytes)
    #   prof_active:true  - 立即激活分析
    # 访问 /debug/pprof/heap 端点可获取内存快照
    export MALLOC_CONF="${MALLOC_CONF:-prof:true,lg_prof_sample:19,prof_active:true}"

    # 仅在交互式终端启用清屏输入（避免 CI/后台卡住）
    if [[ -t 0 && -t 1 ]]; then
        start_server_with_clear "$@"
        return $?
    fi

    exec "$SERVER_BIN" "$@"
}

start_server_with_clear() {
    CLEAR_LOG_FILE="$(mktemp -t ant2api-log.XXXXXX)"

    # 先启动日志跟随，再启动服务，避免丢失初始日志
    tail -n 200 -F "$CLEAR_LOG_FILE" &
    CLEAR_TAIL_PID=$!

    if command -v stdbuf &> /dev/null; then
        stdbuf -oL -eL "$SERVER_BIN" "$@" </dev/null >> "$CLEAR_LOG_FILE" 2>&1 &
    else
        "$SERVER_BIN" "$@" </dev/null >> "$CLEAR_LOG_FILE" 2>&1 &
    fi
    CLEAR_SERVER_PID=$!

    log_info "日志跟随中，输入 clear 后回车可清屏（Ctrl+C 退出）"

    trap 'cleanup_clear_mode; exit 130' INT
    trap 'cleanup_clear_mode; exit 143' TERM

    while kill -0 "$CLEAR_SERVER_PID" 2>/dev/null; do
        if IFS= read -r -t 0.2 cmd; then
            if [[ "$cmd" == "clear" ]]; then
                clear
            elif [[ -n "$cmd" ]]; then
                log_warn "未知命令: $cmd（输入 clear 清屏）"
            fi
        fi
    done

    local server_exit=0
    wait "$CLEAR_SERVER_PID" || server_exit=$?
    cleanup_clear_mode
    return "$server_exit"
}

# =============================================================================
# 主流程
# =============================================================================

main() {
    cd "$SCRIPT_DIR"

    load_env
    check_required_vars

    echo -e "\n${CYAN}━━━ 代码检查 ━━━${NC}\n"

    if needs_rebuild; then
        log_info "检测到代码更新，需要重新构建"
        build_project
    else
        log_success "代码无更新，跳过构建"
    fi

    start_server "$@"
}

main "$@"
