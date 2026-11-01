#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"
profile=${PGO_PROFILE:-release}
host=$(rustc -vV | sed -n 's/^host: //p')
run=${PGO_DIR:-$repo/target/pgo/$profile}
mkdir -p "$run"
run=$(cd "$run" && pwd)
native_run=$run
suffix=
if command -v cygpath >/dev/null 2>&1; then
    native_run=$(cygpath -m "$run")
    suffix=.exe
fi

export CARGO_TARGET_DIR="$native_run/target"
export CARGO_INCREMENTAL=0
binaries=(glua_ls glua_check benchmark)
packages=()
for name in "${binaries[@]}"; do
    packages+=(-p "$name")
done
binary="$run/bin/benchmark_generate$suffix"

build_identity() {
    printf 'repo=%s\nprofile=%s\npackages=%s\n' "$repo" "$profile" "${binaries[*]}"
    rustc -vV
    cargo -V
    env | LC_ALL=C sort | sed -n '/^CARGO_PROFILE_/p'
    git ls-files -cz --others --exclude-standard -- crates vendor tools/benchmark \
        Cargo.toml Cargo.lock 'rust-toolchain*' \
        | LC_ALL=C sort -zu | xargs -0 sha256sum | sha256sum
}

case "${1:-help}" in
    identity)
        build_identity
        ;;
    generate)
        if [[ -e "$run/raw" ]]; then
            printf '%s\n' 'Training data already exists; choose a new PGO_DIR.' >&2
            exit 1
        fi
        mkdir "$run/raw"
        build_identity > "$run/build.identity"
        CARGO_ENCODED_RUSTFLAGS="-Cprofile-generate=$native_run/raw" \
            cargo build --profile "$profile" --target "$host" "${packages[@]}" --bin benchmark
        mkdir -p "$run/bin"
        cp "$run/target/$host/$profile/benchmark$suffix" "$binary"
        sha256sum "$binary"
        ;;
    train)
        : "${BENCH_CODEBASE:?Set BENCH_CODEBASE to a representative workspace.}"
        : "${BENCH_ANNOTATIONS:?Set BENCH_ANNOTATIONS to the annotation workspace.}"
        [[ -d "$run/raw" && -f "$binary" ]]
        LLVM_PROFILE_FILE="$native_run/raw/%m_%p.profraw" "$binary"
        ;;
    merge)
        sysroot=$(rustc --print sysroot)
        if command -v cygpath >/dev/null 2>&1; then
            sysroot=$(cygpath -u "$sysroot")
        fi
        profdata="$sysroot/lib/rustlib/$host/bin/llvm-profdata$suffix"
        if [[ ! -f "$profdata" ]]; then
            printf '%s\n' 'Install the matching LLVM tools with rustup component add llvm-tools-preview.' >&2
            exit 1
        fi
        "$profdata" merge -o "$native_run/merged.profdata" "$native_run/raw"
        ;;
    build)
        [[ -f "$run/merged.profdata" ]]
        if ! build_identity | cmp -s "$run/build.identity" -; then
            printf '%s\n' 'Source, toolchain or profile changed; train again with a new PGO_DIR.' >&2
            exit 1
        fi
        CARGO_ENCODED_RUSTFLAGS="-Cprofile-use=$native_run/merged.profdata"$'\x1f'"-Cllvm-args=-pgo-warn-missing-function" \
            cargo build --profile "$profile" --target "$host" "${packages[@]}"
        mkdir -p "$run/bin"
        for name in "${binaries[@]}"; do
            output="$run/bin/${name}_use$suffix"
            cp "$run/target/$host/$profile/$name$suffix" "$output"
            sha256sum "$output"
        done
        ;;
    help|--help|-h)
        printf '%s\n' \
            'Usage: tools/pgo.sh generate|train|merge|build|identity' \
            'Use one PGO_DIR and PGO_PROFILE throughout all four phases.' \
            'Run train for representative BENCH_CODEBASE and BENCH_ANNOTATIONS inputs.' \
            'Set BENCH_INCREMENTAL=1 to include edit workloads in training.' \
            "Default output: $run/target/$host/$profile"
        ;;
    *)
        printf 'Unknown phase: %s\n' "$1" >&2
        exit 2
        ;;
esac
