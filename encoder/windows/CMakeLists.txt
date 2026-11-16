# Windows build of rayline-arc-encoder: the pinned, patched llama.cpp (static) and the server in one CMake project.
# build.ps1 configures it after verifying the pin and the patches.
cmake_minimum_required(VERSION 3.21)
project(rayline_arc_encoder C CXX)
set(LLAMA_SRC "" CACHE PATH "patched llama.cpp checkout")
set(ENCODER_DIR "${CMAKE_CURRENT_SOURCE_DIR}/.." CACHE PATH "Rayline's encoder/ directory")
set(BUILD_SHARED_LIBS OFF CACHE BOOL "" FORCE)
foreach(o LLAMA_CURL LLAMA_OPENSSL LLAMA_BUILD_TESTS LLAMA_BUILD_EXAMPLES LLAMA_BUILD_TOOLS LLAMA_BUILD_SERVER)
  set(${o} OFF CACHE BOOL "" FORCE)
endforeach()
add_subdirectory(${LLAMA_SRC} llama EXCLUDE_FROM_ALL)
add_executable(rayline-arc-encoder
  ${ENCODER_DIR}/arc_encoder_server.cpp
  ${LLAMA_SRC}/vendor/cpp-httplib/httplib.cpp
  # activeCodePage UTF-8 and longPathAware: narrow paths (argv, std::filesystem, fopen) are UTF-8, so a model under a
  # non-ASCII profile directory opens.
  ${CMAKE_CURRENT_SOURCE_DIR}/rayline-arc-encoder.manifest)
target_include_directories(rayline-arc-encoder PRIVATE ${LLAMA_SRC}/vendor/cpp-httplib ${LLAMA_SRC}/vendor)
target_compile_features(rayline-arc-encoder PRIVATE cxx_std_17)
target_link_libraries(rayline-arc-encoder PRIVATE llama ws2_32)
if(GGML_CUDA)
  # The driver (nvcuda.dll) loads on first use, not at start-up: without it the CUDA runtime finds no device and the
  # server reports a cpu engine, which rayline refuses with a clear error, instead of Windows failing the launch.
  target_link_options(rayline-arc-encoder PRIVATE /DELAYLOAD:nvcuda.dll)
  target_link_libraries(rayline-arc-encoder PRIVATE delayimp)
endif()
