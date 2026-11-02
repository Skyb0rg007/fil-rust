// A C++ class behind a C API. Exceptions that escape reach Rust as errors.
#include <map>
#include <stdexcept>
#include <string>

namespace {

class Inventory {
public:
    void add(const std::string& item, int count)
    {
        if (count <= 0)
            throw std::invalid_argument("count must be positive");
        items_[item] += count;
    }

    void remove(const std::string& item, int count)
    {
        auto it = items_.find(item);
        if (it == items_.end())
            throw std::out_of_range("no such item: " + item);
        if (it->second < count)
            throw std::runtime_error("not enough " + item);
        if ((it->second -= count) == 0)
            items_.erase(it);
    }

    int count(const std::string& item) const
    {
        auto it = items_.find(item);
        return it == items_.end() ? 0 : it->second;
    }

    std::string report() const
    {
        std::string out;
        for (auto& [item, n] : items_)
            out += item + "=" + std::to_string(n) + " ";
        return out;
    }

private:
    std::map<std::string, int> items_;
};

thread_local std::string last_report;

} // namespace

extern "C" {

Inventory* inventory_new() { return new Inventory; }
void inventory_free(Inventory* inv) { delete inv; }
void inventory_add(Inventory* inv, const char* item, int n) { inv->add(item, n); }
void inventory_remove(Inventory* inv, const char* item, int n) { inv->remove(item, n); }
int inventory_count(Inventory* inv, const char* item) { return inv->count(item); }

// Catches inside C++ and reports failure with a status code instead.
int inventory_try_remove(Inventory* inv, const char* item, int n)
{
    try {
        inv->remove(item, n);
        return 0;
    } catch (const std::exception&) {
        return -1;
    }
}

const char* inventory_report(Inventory* inv)
{
    last_report = inv->report();
    return last_report.c_str();
}

}
