# Draft-agreement experiment (scratch branch, not for merge): the teacher-forcing
# driver, linked against llama.cpp cloned at the pinned commit into ./llama.cpp
# by .github/workflows/draft-agreement.yml. CPU only.
cmake_minimum_required(VERSION 3.14)
project(draft_agreement_teacher C CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(BUILD_SHARED_LIBS OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_COMMON OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_TESTS OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_TOOLS OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_EXAMPLES OFF CACHE BOOL "" FORCE)
set(LLAMA_BUILD_SERVER OFF CACHE BOOL "" FORCE)
set(LLAMA_OPENSSL OFF CACHE BOOL "" FORCE)
set(LLAMA_CURL OFF CACHE BOOL "" FORCE)

add_subdirectory(llama.cpp)

add_executable(teacher teacher.cpp)
target_link_libraries(teacher PRIVATE llama)
