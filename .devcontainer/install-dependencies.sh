#!/bin/bash

sudo apt update
sudo apt install -y pkg-config
sudo apt install libzstd-dev
sudo apt install libsqlite3-dev

cargo install cargo-component

export WASI_SDK_VERSION=25.0
ARCH=$(uname -m)
OS=$(uname -s | tr '[:upper:]' '[:lower:]')

if [[ "$OS" == "darwin" && "$ARCH" == "arm64" ]]; then
    WASI_SDK_RELEASE="wasi-sdk-${WASI_SDK_VERSION}-arm64-macos"
elif [[ "$OS" == "darwin" && ( "$ARCH" == "x86_64" || "$ARCH" == "amd64" ) ]]; then
    WASI_SDK_RELEASE="wasi-sdk-${WASI_SDK_VERSION}-x86_64-macos"
elif [[ "$OS" == "linux" && "$ARCH" == "aarch64" ]]; then
    WASI_SDK_RELEASE="wasi-sdk-${WASI_SDK_VERSION}-arm64-linux"
elif [[ "$OS" == "linux" && ( "$ARCH" == "x86_64" || "$ARCH" == "amd64" ) ]]; then
    WASI_SDK_RELEASE="wasi-sdk-${WASI_SDK_VERSION}-x86_64-linux"
else
    echo "Unsupported OS/architecture: $OS/$ARCH"
    exit 1
fi

curl -LO https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-25/${WASI_SDK_RELEASE}.tar.gz
tar xf ${WASI_SDK_RELEASE}.tar.gz
echo "export WASI_SDK_PATH=`pwd`/${WASI_SDK_RELEASE}" >> $HOME/.bashrc
echo "export WIT_REQUIRE_F32_F64=0" >> $HOME/.bashrc