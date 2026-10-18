#!/usr/bin/env bash
# 用假 xcrun 在 Linux 上跑一遍 CI 的「模拟器冒烟」脚本（通过与失败两条路径）。
#
# 为什么值得：真机/模拟器上跑一次要等十几分钟 CI，而脚本本身最容易坏在
# shell 细节上（SIGPIPE、退出码、找不到命令）。docs/10 §6.5 的教训是
# 「先修好观察手段」——这个 harness 就是那条纪律的产物。
#
# 用法：bash tools/test_simulator_smoke.sh
#   （脚本从 .github/workflows/ci.yml 里**提取**真实步骤来跑，不复制粘贴，
#     避免"测试的和 CI 跑的不是同一份"）

set -uo pipefail
cd "$(dirname "$0")/.."
repo=$PWD

scene=${1:-pass}   # pass | fail
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "==== 场景：$scene（临时目录 $tmp）===="

# 1) 提取 CI 里那一步的 run 脚本
python3 - "$tmp/smoke.sh" <<'PY'
import sys, yaml
doc = yaml.safe_load(open('.github/workflows/ci.yml'))
for step in doc['jobs']['ios-macos']['steps']:
    if step.get('name', '').startswith('模拟器冒烟'):
        open(sys.argv[1], 'w').write(step['run'])
        print(f'  已提取步骤脚本：{len(step["run"].splitlines())} 行')
        break
else:
    raise SystemExit('没找到「模拟器冒烟」步骤')
PY

# 2) 造一个假的 workspace：只有脚本要求的产物路径
ws="$tmp/ws"
mkdir -p "$ws/app/build/ios/iphonesimulator/Runner.app"
echo '<plist/>' > "$ws/app/build/ios/iphonesimulator/Runner.app/Info.plist"

# 3) 假 xcrun + 假 sleep（让 120 秒的等待循环瞬间跑完）
bin="$tmp/bin"
mkdir -p "$bin"

cat > "$bin/xcrun" <<EOF
#!/usr/bin/env bash
set -uo pipefail
args="\$*"
case "\$args" in
  "simctl list devices available")
    cat <<'LIST'
-- iOS 26.0 --
    iPhone 17 Pro (11111111-2222-3333-4444-555555555555) (Shutdown)
    iPhone 17 (AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE) (Shutdown)
-- iOS 18.5 --
    iPad Pro 13-inch (M4) (99999999-2222-3333-4444-555555555555) (Shutdown)
LIST
    ;;
  *"simctl boot "*|*"simctl bootstatus"*|*"simctl install"*|*"simctl terminate"*)
    exit 0
    ;;
  *"simctl io "*"screenshot"*)
    # 最后一个参数是输出路径
    out="\${@: -1}"
    printf 'PNG(dummy)' > "\$out"
    echo "Wrote screenshot to \$out"
    ;;
  *"simctl launch"*)
    if [ "$scene" = "pass" ]; then
      echo "[perch-selfcheck] ok core=0.1.0"
      echo "flutter: [perch] 打开成功：812 ms"
    else
      echo "flutter: 应用启动中……"
      echo "Lost connection to device."
    fi
    sleep 300
    ;;
  *"simctl spawn"*"log show"*)
    echo "（假系统日志：没有自检标记）"
    ;;
  *"simctl spawn"*"launchctl list"*)
    echo "1234  0  com.perch.perch"
    ;;
  *)
    echo "（假 xcrun 未处理：\$args）" >&2
    exit 0
    ;;
esac
EOF
chmod +x "$bin/xcrun"

cat > "$bin/sleep" <<'EOF'
#!/usr/bin/env bash
# 假 sleep：把等待压缩成 0 秒，加速本地验证
exit 0
EOF
chmod +x "$bin/sleep"

# 4) 跑真实步骤脚本（在假 workspace 里跑：CI 的工作目录就是仓库根）
echo "---- 开始执行 ----"
set +e
( cd "$ws" && PATH="$bin:$PATH" bash "$tmp/smoke.sh" ) > "$tmp/out.log" 2>&1
code=$?
set -e

echo "退出码：$code（期望：pass=0 / fail=1）"
echo "---- 输出（尾 25 行）----"
tail -25 "$tmp/out.log"

echo "---- 产物 ----"
ls -la "$ws/ci-artifacts" 2>/dev/null || echo "（没有 ci-artifacts 目录）"

expect=1
[ "$scene" = "pass" ] && expect=0
if [ "$code" != "$expect" ]; then
  echo "✗ 退出码不符：期望 $expect，实际 $code"
  exit 1
fi

if [ "$scene" = "pass" ]; then
  if [ ! -f "$ws/ci-artifacts/home.png" ]; then
    echo "✗ 通过场景应当产出一张截图"
    exit 1
  fi
  if [ ! -f "$ws/ci-artifacts/app-console.log" ]; then
    echo "✗ 通过场景应当留下控制台日志"
    exit 1
  fi
  echo "✓ 通过场景：退出码 0 + 截图 + 控制台日志都在"
else
  # 失败场景要能**说出原因**，而不是静默退出
  if ! grep -q '没有出现 \[perch-selfcheck\] 日志' "$tmp/out.log"; then
    echo "✗ 失败场景应当打印明确原因"
    exit 1
  fi
  echo "✓ 失败场景：退出码 1 + 打印了可读的失败原因"
fi
