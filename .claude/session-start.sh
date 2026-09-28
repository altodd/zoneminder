#!/bin/bash
# SessionStart hook: prepares a Claude Code cloud session. No-op locally.
#
# System packages and the toolchain come from the environment setup script
# (.claude/cloud-env-setup.sh). This covers what depends on the checkout.

[ "$CLAUDE_CODE_REMOTE" = "true" ] || exit 0

cd "$CLAUDE_PROJECT_DIR" || exit 0

[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

# C++ build pulls in dep/CxxUrl and dep/RtspServer.
git submodule update --init --recursive >/dev/null 2>&1 || true

# ESLint with the same packages CI installs (package.json is gitignored).
if [ ! -d node_modules/eslint-config-google ]; then
  npm install --no-audit --no-fund --silent \
    eslint@9 @eslint/eslintrc @eslint/js@"<10" globals \
    eslint-config-google@0.14.0 eslint-plugin-html eslint-plugin-php-markup@6.0.0 \
    >/dev/null 2>&1 || true
fi

# zmng: fetch crates now, then warm the test build in the background so the
# first `cargo test` doesn't pay the full cold compile.
if [ -f zmng/Cargo.toml ]; then
  (cd zmng && cargo fetch --locked >/dev/null 2>&1) || true
  (cd zmng && nohup cargo test --no-run --locked >/tmp/zmng-warm-build.log 2>&1 &) || true
fi

cat <<'EOF'
Cloud session notes: cameras, the ZoneMinder host and its MySQL are not
reachable from here. zmng can be exercised with a temp config and a
synthetic camera: --sub-url "lavfi:testsrc=size=640x360:rate=5".
A background zmng test build is warming (log: /tmp/zmng-warm-build.log).
EOF

exit 0
