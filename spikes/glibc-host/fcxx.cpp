#include <cstdio>
#include <dlfcn.h>
#include <stdexcept>
#include <string>
#include <vector>
static int thrower(int x) { if (x > 2) throw std::runtime_error("boom " + std::to_string(x)); return x; }
int main(int, char **argv) {
    std::vector<std::string> v;
    try { for (int i = 0; i < 5; i++) v.push_back(std::to_string(thrower(i))); }
    catch (const std::exception &e) { std::printf("caught: %s after %zu items\n", e.what(), v.size()); }
    std::string p = std::string(argv[1]) + "/libadd.so";
    void *h = dlopen(p.c_str(), RTLD_NOW);
    if (!h) { std::printf("dlopen failed: %s\n", dlerror()); return 1; }
    auto add = (int (*)(int, int))dlsym(h, "add");
    auto up = (char *(*)(const char *))dlsym(h, "dup_upper");
    std::printf("dlopen'd add(40,2)=%d dup_upper=%s\n", add(40, 2), up("shout"));
    return 0;
}
