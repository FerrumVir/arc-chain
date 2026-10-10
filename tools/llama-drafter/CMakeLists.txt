# The llama.cpp drafter for crates/arc-inference/src/draft_verify.rs
# (ProcessDrafter's line protocol), linked against llama.cpp checked out at
# release b11534 into ./llama.cpp by .github/workflows/draft-verify-bench.yml.
# CPU only. Not part of any ARC binary or release.
cmake_minimum_required(VERSION 3.14)
project(llama_drafter C CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(BUILD_SHARED_LIBS OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_COMMON OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_TESTS OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_TOOLS OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_EXAMPLES OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_SERVER OFF CACHE BOOL "" FORCE)
set(LLAMA_OPENSSL OFF CACHE BOOL "" FORCE)

add_subdirectory(llama.cpp)

add_executable(llama-drafter llama_drafter.cpp)
target_link_libraries(llama-drafter PRIVATE llama)
