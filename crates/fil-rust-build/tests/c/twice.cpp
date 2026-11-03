#include <vector>
#include "greet.h"

extern "C" int twice_scaled(int x) {
    std::vector<int> v{scaled(x), scaled(x)};
    return v[0] + v[1];
}
