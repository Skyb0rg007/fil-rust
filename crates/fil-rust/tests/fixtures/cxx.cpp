// Test fixture: C++ behind a C ABI, including exceptions.

#include <algorithm>
#include <map>
#include <memory>
#include <numeric>
#include <stdexcept>
#include <string>
#include <thread>
#include <vector>

extern "C" {

int checked_div(int a, int b) {
    if (b == 0) throw std::runtime_error("division by zero");
    return a / b;
}

int catch_inside(int x) {
    try {
        if (x < 0) throw std::invalid_argument("negative");
        return x;
    } catch (const std::exception&) {
        return -1;
    }
}

int call_and_catch(int (*f)(int), int x) {
    try {
        return f(x);
    } catch (...) {
        return -999;
    }
}

struct Counter {
    std::vector<int> values;
    std::map<std::string, int> names;
};

Counter* counter_new() { return new Counter(); }
void counter_push(Counter* c, int v) { c->values.push_back(v); }
long counter_sum(const Counter* c) { return std::accumulate(c->values.begin(), c->values.end(), 0L); }
void counter_name(Counter* c, const char* name, int v) { c->names[name] = v; }
int counter_lookup(const Counter* c, const char* name) {
    auto it = c->names.find(name);
    return it == c->names.end() ? -1 : it->second;
}
void counter_free(Counter* c) { delete c; }

char* join_sorted(const char** words, int n) {
    std::vector<std::string> v(words, words + n);
    std::sort(v.begin(), v.end());
    std::string out;
    for (auto& w : v) out += (out.empty() ? "" : ",") + w;
    char* r = (char*)malloc(out.size() + 1);
    std::copy(out.begin(), out.end(), r);
    r[out.size()] = 0;
    return r;
}

long parallel_sum(int nthreads, int per) {
    std::vector<long> partial(nthreads);
    std::vector<std::thread> threads;
    for (int t = 0; t < nthreads; t++)
        threads.emplace_back([&, t] {
            auto data = std::make_unique<std::vector<int>>(per);
            std::iota(data->begin(), data->end(), t * per);
            partial[t] = std::accumulate(data->begin(), data->end(), 0L);
        });
    for (auto& th : threads) th.join();
    return std::accumulate(partial.begin(), partial.end(), 0L);
}

}

static int destructor_runs;

struct Guard {
    ~Guard() { destructor_runs++; }
};

extern "C" int throw_with_guards(int depth) {
    Guard g;
    if (depth == 0) throw std::logic_error("deep");
    return throw_with_guards(depth - 1) + 1;
}

extern "C" int get_destructor_runs(void) { return destructor_runs; }

extern "C" int throw_int(int v) { throw v; }

// Calls f with a destructor-carrying local live and a handler ready.
extern "C" int guarded_call(int (*f)(int), int x) {
    Guard g;
    try {
        return f(x);
    } catch (...) {
        return -1000;
    }
}

// Calls f on a new std::thread; an exception there is caught there.
extern "C" int call_on_thread(int (*f)(int), int x) {
    int result = 0;
    std::thread t([&] {
        try {
            result = f(x);
        } catch (...) {
            result = -2000;
        }
    });
    t.join();
    return result;
}

struct CollectsOnTheWayOut {
    void (*collect)();
    ~CollectsOnTheWayOut() { collect(); }
};

static void throw_past_collections(void (*collect)(), int depth) {
    CollectsOnTheWayOut guard { collect };
    if (depth == 0) throw std::runtime_error("kept");
    throw_past_collections(collect, depth - 1);
}

// Between frames, only the unwinder knows about the exception.
extern "C" int collect_while_unwinding(void (*collect)()) {
    try {
        throw_past_collections(collect, 3);
    } catch (const std::exception& error) {
        return std::string(error.what()) == "kept";
    }
    return -1;
}
