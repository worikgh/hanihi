cmake_minimum_required(VERSION 3.16)
project(cpp_fixture CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

enable_testing()

add_library(foo src/foo.cpp)
target_include_directories(foo PUBLIC ${CMAKE_CURRENT_SOURCE_DIR}/include)

add_executable(foo_test src/foo_test.cpp)
target_link_libraries(foo_test PRIVATE foo)

add_test(NAME foo_add COMMAND foo_test)
