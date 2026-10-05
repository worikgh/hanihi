// Test driver for the fixture library. Exits non-zero when the assertion
// fails, which is what makes `ctest` meaningfully red on a deliberate break.

#include <cstdio>

#include "foo.hpp"

int main() {
    if (fixture::add(2, 3) != 5) {
        std::fprintf(stderr, "add(2, 3) != 5\n");
        return 1;
    }
    if (fixture::add(-1, 1) != 0) {
        std::fprintf(stderr, "add(-1, 1) != 0\n");
        return 1;
    }
    std::printf("all assertions passed\n");
    return 0;
}
