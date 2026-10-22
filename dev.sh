#!/usr/bin/env bash
# Development commands, user installation, and local Codex MCP setup.
set -euo pipefail

readonly PRODUCT_ROOT="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
readonly INVOCATION_ROOT="$PWD"

usage() {
    cat <<'EOF'
Usage:
  ./dev.sh <command> [args]

Commands:
  help        Show these commands (also the default with no arguments).
  doctor      Check the toolchain, lockfile, and installed commands.
  install     Build a release binary and install a new copy for your user.
  reinstall   Rebuild and atomically replace the installed binary.
  uninstall   Remove the binary; keep models, indexes, and Codex config.
  develop     Fetch the locked Rust dependencies for development.
  run ...     Build and run this checkout; forward arguments to Lore.
  check       Check formatting and Clippy without rewriting source files.
  fmt         Format the Rust code.
  test ...    Run Cargo tests; with no arguments, also run installer tests.
  build       Build the release binary in target/release/lore.
  setup PATH  Prepare a repository's index and register its MCP server in Codex.
  refresh PATH  Refresh that index using existing local model assets.

Builds and checks run in the checkout. run keeps your working directory.
Extra test arguments go to cargo test. install/reinstall still accept --prod.

Setup / refresh options:
  --name NAME         MCP name (default: lore-<repository-directory-name>)
  --index-dir DIR     Index directory (default: <Lore checkout>/.lore/mcp/<name>)
  --model-dir DIR     Shared assets (default: ~/.lore/models/bge-small)
Setup-only options:
  --usage-config FILE Optional source profile, selectable per query as 'usage'
  --replace           Replace an existing named Codex MCP entry

Repeat custom --name, --index-dir, and --model-dir options with refresh; options
are not saved as a separate refresh profile. Relative paths resolve from your
working directory. Use unique names for repositories with the same basename.

Environment:
  LORE_DEV_INSTALL_DIR  Binary directory (default: $CARGO_HOME/bin or ~/.cargo/bin)
  CARGO_TARGET_DIR      Absolute build directory (default: Lore's target)

Example:
  ./dev.sh doctor
  ./dev.sh install
  ./dev.sh setup /path/to/project
  ./dev.sh run --help
  ./dev.sh test --test retrieval

Requires Bash, Rust 1.88+, and a native C/C++ toolchain for builds; setup also
requires Codex CLI. check/fmt need Clippy/rustfmt; installer tests need Python 3.
Setup provisions pinned model assets (~67 MB) if needed;
valid assets are reused across indexes. Refresh is local and needs existing assets.
Successful index/status reports are saved in INDEX_DIR as index-report.json and
status-report.json. Codex starts Lore itself; restart Codex after setup/reinstall.
Linux x86-64 is the exercised platform. This script does not modify shell PATH.
EOF
}

fail() {
    printf 'lore dev installer: %s\n' "$*" >&2
    exit 1
}

usage_error() {
    printf 'lore dev: %s\n' "$*" >&2
    exit 2
}

require_commands() {
    local command
    for command in "$@"; do
        command -v "$command" >/dev/null 2>&1 || fail "required command is unavailable: $command"
    done
}

absolute_path() {
    [[ -n "$1" && "$1" != *$'\n'* ]] || fail 'paths must be nonempty and contain no newlines'
    case "$1" in
        /*) printf '%s\n' "$1" ;;
        *) printf '%s/%s\n' "$INVOCATION_ROOT" "$1" ;;
    esac
}

resolve_install_directory() {
    INSTALL_DIRECTORY="${LORE_DEV_INSTALL_DIR:-${CARGO_HOME:-${HOME:?HOME is required}/.cargo}/bin}"
    [[ "$INSTALL_DIRECTORY" == /* && "$INSTALL_DIRECTORY" != / && "$INSTALL_DIRECTORY" != *$'\n'* ]] ||
        fail 'LORE_DEV_INSTALL_DIR must be an absolute path other than /, without newlines'
    [[ ! -L "$INSTALL_DIRECTORY" ]] || fail "refusing a symbolic-link install directory: $INSTALL_DIRECTORY"
    [[ ! -e "$INSTALL_DIRECTORY" || -d "$INSTALL_DIRECTORY" ]] || fail "not a directory: $INSTALL_DIRECTORY"
    BINARY="$INSTALL_DIRECTORY/lore"
}

verify_binary() {
    local version
    [[ -f "$1" && ! -L "$1" && -x "$1" ]] || fail "missing or unsafe Lore executable: $1"
    version="$("$1" --version)" || fail "version probe failed: $1"
    [[ "$version" == 'lore '* ]] || fail "unexpected executable identity at $1: $version"
}

preflight_destination() {
    if [[ -e "$BINARY" || -L "$BINARY" ]]; then
        [[ "$COMMAND" != install ]] || fail "$BINARY already exists; use './dev.sh reinstall'"
        verify_binary "$BINARY"
    fi
}

resolve_build_directory() {
    BUILD_DIRECTORY="${CARGO_TARGET_DIR:-$PRODUCT_ROOT/target}"
    [[ "$BUILD_DIRECTORY" == /* && "$BUILD_DIRECTORY" != / && "$BUILD_DIRECTORY" != *$'\n'* ]] ||
        fail 'CARGO_TARGET_DIR must be an absolute path other than /, without newlines'
}

build_product() {
    local profile="$1"
    local -a profile_args=()
    require_commands cargo
    resolve_build_directory
    if [[ "$profile" == release ]]; then
        profile_args=(--release)
    fi
    (
        cd -- "$PRODUCT_ROOT"
        cargo build --manifest-path "$PRODUCT_ROOT/Cargo.toml" \
            --locked "${profile_args[@]}" --bin lore --target-dir "$BUILD_DIRECTORY"
    )
    verify_binary "$BUILD_DIRECTORY/$profile/lore"
}

install_product() {
    local staging_directory
    require_commands cargo install mkdir mktemp mv rm rmdir
    preflight_destination
    build_product release
    mkdir -p -- "$INSTALL_DIRECTORY"
    [[ ! -L "$INSTALL_DIRECTORY" ]] || fail "install directory became a symbolic link: $INSTALL_DIRECTORY"
    preflight_destination
    staging_directory="$(mktemp -d "$INSTALL_DIRECTORY/.lore-dev-install.XXXXXX")"
    (
        trap 'rm -f -- "$staging_directory/lore"; rmdir -- "$staging_directory"' EXIT
        install -m 0755 -- "$BUILD_DIRECTORY/release/lore" "$staging_directory/lore"
        verify_binary "$staging_directory/lore"
        mv -f -- "$staging_directory/lore" "$BINARY"
    )
    printf 'Installed %s\n' "$BINARY"
    case ":${PATH:-}:" in
        *":$INSTALL_DIRECTORY:"*) ;;
        *) printf 'For shell use, add %s to PATH. Codex uses the absolute executable path.\n' "$INSTALL_DIRECTORY" ;;
    esac
    if [[ "$COMMAND" == reinstall ]]; then
        printf '%s\n' 'Restart existing Codex sessions to load the new executable.'
    fi
}

doctor() {
    local status=0 tool location rust_version component
    printf 'Checkout: %s\n' "$PRODUCT_ROOT"
    for tool in Cargo.toml Cargo.lock; do
        if [[ -f "$PRODUCT_ROOT/$tool" ]]; then
            printf '%s: found\n' "$tool"
        else
            printf '%s: MISSING\n' "$tool"
            status=1
        fi
    done
    for tool in cargo rustc cc c++ python3; do
        if location="$(command -v "$tool")"; then
            printf '%s: %s\n' "$tool" "$location"
        else
            printf '%s: MISSING\n' "$tool"
            status=1
        fi
    done
    if command -v rustc >/dev/null 2>&1; then
        if rust_version="$(cd -- "$PRODUCT_ROOT"; rustc --version)"; then
            printf '%s\n' "$rust_version"
            if [[ "$rust_version" =~ ^rustc\ ([0-9]+)\.([0-9]+)\. ]] &&
                (( BASH_REMATCH[1] > 1 || (BASH_REMATCH[1] == 1 && BASH_REMATCH[2] >= 88) )); then
                :
            else
                printf '%s\n' 'Rust 1.88+ is required.'
                status=1
            fi
        else
            status=1
        fi
    fi
    if command -v cargo >/dev/null 2>&1; then
        (cd -- "$PRODUCT_ROOT"; cargo --version) || status=1
        for component in fmt clippy; do
            (cd -- "$PRODUCT_ROOT"; cargo "$component" --version) || {
                printf 'Missing Rust component; install with: rustup component add %s\n' \
                    "${component/fmt/rustfmt}"
                status=1
            }
        done
    fi
    printf 'codex (optional, for setup): %s\n' "$(command -v codex || printf 'not found')"
    printf 'lore on PATH: %s\n' "$(command -v lore || printf 'not installed or not on PATH')"
    return "$status"
}

check_product() {
    local format_status=0 lint_status=0
    require_commands cargo
    resolve_build_directory
    (cd -- "$PRODUCT_ROOT"; cargo fmt --all --check) || format_status=$?
    (cd -- "$PRODUCT_ROOT"; cargo clippy --all-targets --locked \
        --target-dir "$BUILD_DIRECTORY" -- -D warnings) || lint_status=$?
    if ((format_status != 0)); then
        return "$format_status"
    fi
    return "$lint_status"
}

test_product() {
    require_commands cargo
    if (($# == 0)); then
        require_commands python3
    fi
    resolve_build_directory
    (
        cd -- "$PRODUCT_ROOT"
        cargo test --locked --target-dir "$BUILD_DIRECTORY" "$@"
        if (($# == 0)); then
            python3 -B scripts/test_dev_installer.py
        fi
    )
}

save_report() {
    local report="$1"
    shift
    printf 'Running Lore preparation (report: %s)\n' "$report"
    (
        temporary_report="$(mktemp "$report.XXXXXX")"
        trap 'rm -f -- "$temporary_report"' EXIT
        "$@" > "$temporary_report"
        mv -f -- "$temporary_report" "$report"
    )
}

prepare_repository() {
    local repository name='' index_dir='' model_dir='' usage_config='' replace=false
    (($# > 0)) || fail "$COMMAND requires a repository path; see --help"
    repository="$1"
    shift
    while (($#)); do
        case "$1" in
            --name|--index-dir|--model-dir|--usage-config)
                (($# >= 2)) && [[ -n "$2" && "$2" != --* ]] || fail "$1 requires a value"
                case "$1" in
                    --name) name="$2" ;;
                    --index-dir) index_dir="$2" ;;
                    --model-dir) model_dir="$2" ;;
                    --usage-config) usage_config="$2" ;;
                esac
                shift 2
                ;;
            --replace) replace=true; shift ;;
            *) fail "unknown $COMMAND option: $1" ;;
        esac
    done
    [[ "$COMMAND" == setup || ( "$replace" == false && -z "$usage_config" ) ]] ||
        fail '--replace and --usage-config only apply to setup'
    repository="$(absolute_path "$repository")"
    [[ -d "$repository" ]] || fail "repository directory does not exist: $repository"
    repository="$(CDPATH= cd -- "$repository" && pwd -P)"
    require_commands tr mkdir mktemp mv rm
    if [[ -z "$name" ]]; then
        name="lore-$(printf '%s' "${repository##*/}" | LC_ALL=C tr -c 'a-zA-Z0-9_-' '-')"
    fi
    [[ "$name" =~ ^[a-zA-Z0-9_][a-zA-Z0-9_-]*$ ]] ||
        fail '--name must start with a letter, digit, or underscore and contain only letters, digits, underscores, or hyphens'
    index_dir="$(absolute_path "${index_dir:-$PRODUCT_ROOT/.lore/mcp/$name}")"
    [[ "$index_dir" != / && ! -L "$index_dir" && ( ! -e "$index_dir" || -d "$index_dir" ) ]] ||
        fail "index directory must be a directory other than /, not a symbolic link: $index_dir"
    if [[ -z "$model_dir" ]]; then
        [[ "${HOME:-}" == /* ]] || fail 'HOME must be an absolute path; alternatively supply --model-dir'
        model_dir="$HOME/.lore/models/bge-small"
    fi
    model_dir="$(absolute_path "$model_dir")"
    [[ "$model_dir" != / && ( ! -e "$model_dir" || -d "$model_dir" ) ]] ||
        fail "model path must be a directory other than /: $model_dir"
    if [[ -n "$usage_config" ]]; then
        usage_config="$(absolute_path "$usage_config")"
        [[ -f "$usage_config" && -r "$usage_config" ]] || fail "cannot read usage config: $usage_config"
    fi
    verify_binary "$BINARY"
    if [[ "$COMMAND" == setup ]]; then
        require_commands codex
        # Check configuration before provisioning; a broken config is not an
        # absent server, and an existing named connection needs explicit replacement.
        codex mcp list >/dev/null
        if [[ "$replace" == false ]] && codex mcp get "$name" >/dev/null 2>&1; then
            fail "Codex entry '$name' already exists; use refresh, another --name, or setup --replace"
        fi
    else
        [[ -d "$model_dir" ]] || fail "model assets are missing: $model_dir; run setup or supply --model-dir"
    fi
    mkdir -p -- "$index_dir"
    index_dir="$(CDPATH= cd -- "$index_dir" && pwd -P)"
    printf 'Preparing %s\nIndex: %s\n' "$repository" "$index_dir"
    if [[ "$COMMAND" == setup ]]; then
        "$BINARY" --index-dir "$index_dir" --model-dir "$model_dir" model install
    fi
    save_report "$index_dir/index-report.json" \
        "$BINARY" --index-dir "$index_dir" --model-dir "$model_dir" index "$repository" --json
    save_report "$index_dir/status-report.json" \
        "$BINARY" --index-dir "$index_dir" --model-dir "$model_dir" status --check --json
    if [[ "$COMMAND" == setup ]]; then
        local -a server_args=(--index-dir "$index_dir" --model-dir "$model_dir" mcp)
        if [[ -n "$usage_config" ]]; then
            server_args+=(--usage-config "$usage_config")
        fi
        codex mcp add "$name" -- "$BINARY" "${server_args[@]}"
        printf 'Configured Codex MCP: %s\n' "$name"
        printf '%s\n' 'Start a new Codex session in the repository and check /mcp.'
    else
        printf '%s\n' 'Index refreshed. Existing MCP processes observe the new index on their next call.'
    fi
}

COMMAND="${1:-help}"
if (($# > 0)); then shift; fi
case "$COMMAND" in
    help|--help|-h|doctor|install|reinstall|uninstall|develop|run|check|fmt|test|build|setup|refresh) ;;
    *) usage_error "unknown command: $COMMAND; see --help" ;;
esac
if [[ "$COMMAND" != run && "$COMMAND" != test ]] && (($# == 1)) &&
    [[ "$1" == --help || "$1" == -h ]]; then
    usage
    exit 0
fi
case "$COMMAND" in
    help|--help|-h) (($# == 0)) || usage_error 'help does not accept arguments'; usage ;;
    install|reinstall)
        (($# == 0)) || { (($# == 1)) && [[ "$1" == --prod ]]; } || \
            usage_error "usage: ./dev.sh $COMMAND [--prod]"
        resolve_install_directory
        install_product
        ;;
    uninstall)
        (($# == 0)) || usage_error 'usage: ./dev.sh uninstall'
        resolve_install_directory
        preflight_destination
        rm -f -- "$BINARY"
        printf 'Removed %s; indexes, models, and Codex configuration were preserved.\n' "$BINARY"
        printf '%s\n' 'Remove an unused connection separately with: codex mcp remove <name>'
        ;;
    setup|refresh)
        resolve_install_directory
        prepare_repository "$@"
        ;;
    run)
        build_product debug
        exec "$BUILD_DIRECTORY/debug/lore" "$@"
        ;;
    test) test_product "$@" ;;
    doctor|develop|check|fmt|build)
        (($# == 0)) || usage_error "$COMMAND does not accept arguments; see --help"
        case "$COMMAND" in
            doctor) doctor ;;
            develop)
                require_commands cargo
                (cd -- "$PRODUCT_ROOT"; cargo fetch --locked)
                ;;
            check) check_product ;;
            fmt)
                require_commands cargo
                (cd -- "$PRODUCT_ROOT"; cargo fmt --all)
                ;;
            build)
                build_product release
                printf 'Built %s\n' "$BUILD_DIRECTORY/release/lore"
                ;;
        esac
        ;;
esac
