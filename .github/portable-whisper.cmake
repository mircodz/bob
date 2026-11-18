# Release binaries run on other people's machines, so CI builds whisper.cpp for
# a common CPU baseline (AVX2 on x86-64) instead of tuning it to the runner's
# CPU, which could crash with an illegal instruction elsewhere. whisper-rs-sys
# passes CMAKE_* environment variables to CMake, and CMAKE_PROJECT_INCLUDE_BEFORE
# loads this file before whisper.cpp declares its options.
set(GGML_NATIVE OFF CACHE BOOL "Optimize ggml for the build machine's CPU" FORCE)
