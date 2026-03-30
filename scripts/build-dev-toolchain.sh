#!/bin/bash
set -euo pipefail

# Dev toolchain builder for Amazon Linux 2023
# Installs: gcc, ninja, python3, htop, git, rust/cargo, cmake (from source), clang (from source)

NPROC=$(nproc)
echo "=== Building dev toolchain (${NPROC} cores) ==="

# System packages
echo "=== Installing system packages ==="
sudo dnf install -y gcc gcc-c++ ninja-build python3 htop git tar gzip

# Rust + cargo via rustup
echo "=== Installing Rust ==="
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustc --version
cargo --version

# CMake from source (latest release)
echo "=== Building CMake from source ==="
cd /tmp
git clone --depth 1 --branch v3.31.6 https://github.com/Kitware/CMake.git cmake-src
cd cmake-src
./bootstrap --parallel="${NPROC}" --prefix=/usr/local
make -j"${NPROC}"
sudo make install
cd /
rm -rf /tmp/cmake-src
cmake --version

# LLVM/Clang from source (latest release)
echo "=== Building Clang/LLVM from source ==="
cd /tmp
git clone --depth 1 --branch llvmorg-19.1.7 https://github.com/llvm/llvm-project.git llvm-src
cmake -S llvm-src/llvm -B llvm-build -G Ninja \
    -DCMAKE_BUILD_TYPE=Release \
    -DLLVM_ENABLE_PROJECTS="clang;lld" \
    -DCMAKE_INSTALL_PREFIX=/usr/local \
    -DLLVM_TARGETS_TO_BUILD="AArch64;X86;WebAssembly" \
    -DLLVM_ENABLE_ASSERTIONS=OFF \
    -DLLVM_PARALLEL_LINK_JOBS=2
ninja -C llvm-build -j"${NPROC}"
sudo ninja -C llvm-build install
cd /
rm -rf /tmp/llvm-src /tmp/llvm-build
clang --version

echo "=== Dev toolchain build complete ==="
echo "Installed: gcc, ninja, python3, htop, git, rust, cargo, cmake, clang, lld"
