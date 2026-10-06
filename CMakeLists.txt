cmake_minimum_required(VERSION 3.20)
project(pure-decomp LANGUAGES C CXX)

set(XENON_RECOMP_SOURCE_DIR "${CMAKE_CURRENT_SOURCE_DIR}/local/tools/XenonRecomp"
    CACHE PATH "Path to a recursive XenonRecomp checkout")
if(NOT EXISTS "${XENON_RECOMP_SOURCE_DIR}/XenonUtils/CMakeLists.txt")
    message(FATAL_ERROR "Clone XenonRecomp and its submodules; see docs/tooling.md")
endif()

add_subdirectory("${XENON_RECOMP_SOURCE_DIR}" "${CMAKE_BINARY_DIR}/xenon" EXCLUDE_FROM_ALL)
add_executable(inspect_xex tools/inspect_xex.cpp)
target_compile_features(inspect_xex PRIVATE cxx_std_20)
target_compile_options(inspect_xex PRIVATE -Wall -Wextra -Wpedantic)
target_link_libraries(inspect_xex PRIVATE XenonUtils)
