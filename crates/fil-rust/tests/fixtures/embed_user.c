// Embedded by tests/embedded.rs, linked against embed_base.c's library.
int embed_base_value(void);

int embed_user_value(void) { return embed_base_value() + 1; }
