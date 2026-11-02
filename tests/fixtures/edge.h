/* Edge cases for fil-rust-bindgen: constructs it must leave out, and
   typedef and pointer shapes it must spell right. Only declarations; the
   generated bindings (tests/generated/edge.rs) are compiled, not run. */

struct flags { unsigned a : 3; unsigned b : 5; };
union num { int i; float f; };
struct packet { int len; char data[]; };
struct holder { union num n; };
typedef struct holder holder_t;
struct ok { int x; };
typedef int (*vfn)(int, ...);

void take_flags(struct flags f);
void take_flags_ptr(struct flags* f);
void take_union(union num n);
void take_holder_ptr(struct holder* h);
void take_holder_t_ptr(holder_t* h);
long double precise(void);
int logf_(const char* fmt, ...);
void with_vfn(vfn f);
int fine(struct ok o, union num* n);

typedef char* str;
typedef struct node { str label; struct node* kids[2]; } node_t;
typedef void (*cb)(void* user, const node_t* n);
struct with_cb { cb on_visit; void (*raw)(void); void* user; };
extern const char* const names[3];
str* split(str s, int* count);
void walk(node_t* root, cb f, void* user);
int fil(int fil, int self_);
