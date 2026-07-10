# SPDX-License-Identifier: Apache-2.0
#
# rseq MCU firmware (C + Rust). rust_cargo_application() builds the rustapp
# staticlib and provides main()→rust_main(); the C files are the FFI shims.

cmake_minimum_required(VERSION 3.20.0)

find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})

project(rseq_nucleo)

rust_cargo_application()

target_sources(app PRIVATE
  src/zephyr_transport_ffi.c
  src/zephyr_bus_ffi.c
)
