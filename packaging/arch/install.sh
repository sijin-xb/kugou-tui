#!/usr/bin/env bash
#
# 给 /etc/pacman.conf 追加 kugou-tui 仓库段。幂等，重复运行无副作用。
#
# 用法：curl -fsSL https://sijin-xb.github.io/kugou-tui-repo/install.sh | sudo bash
#
# 通过管道执行时 $0 是 bash 而非本文件路径，所以这里不读任何相对路径，
# 也不做自我更新——URL 只有下面这一处。
set -euo pipefail

REPO_NAME='kugou-tui'
REPO_URL='https://sijin-xb.github.io/kugou-tui-repo/$arch'
PACMAN_CONF='/etc/pacman.conf'

die() {
	printf '\033[31m错误：%s\033[0m\n' "$1" >&2
	exit 1
}

[ "$(id -u)" -eq 0 ] || die "需要 root。用：curl -fsSL <url> | sudo bash"

[ -r /etc/os-release ] || die "读不到 /etc/os-release，无法判断发行版"
# shellcheck disable=SC1091
. /etc/os-release
case " ${ID:-} ${ID_LIKE:-} " in
	*' arch '*) ;;
	*) die "这个脚本只支持 Arch 系发行版（当前 ID=${ID:-?} ID_LIKE=${ID_LIKE:-?}）" ;;
esac

command -v pacman >/dev/null 2>&1 || die "找不到 pacman"

[ -f "$PACMAN_CONF" ] || die "找不到 $PACMAN_CONF"

if grep -q "^\[${REPO_NAME}\]" "$PACMAN_CONF"; then
	printf '\033[36m[%s] 已在 %s 中，跳过。\033[0m\n' "$REPO_NAME" "$PACMAN_CONF"
else
	[ -w "$PACMAN_CONF" ] || die "$PACMAN_CONF 不可写"
	cp -a "$PACMAN_CONF" "${PACMAN_CONF}.bak.$(date +%Y%m%d%H%M%S)"
	# $arch 必须原样写入，由 pacman 展开。用单引号 heredoc 阻止 shell 展开。
	cat >>"$PACMAN_CONF" <<'EOF'

[kugou-tui]
SigLevel = Optional TrustAll
Server = https://sijin-xb.github.io/kugou-tui-repo/$arch
EOF
	printf '\033[32m已追加 [%s] 到 %s（原文件已备份）。\033[0m\n' "$REPO_NAME" "$PACMAN_CONF"
fi

cat <<'EOF'

下一步：

  sudo pacman -Sy
  sudo pacman -S kugou-tui

装完直接敲 kugou-tui 即可，不需要启动脚本。
EOF
