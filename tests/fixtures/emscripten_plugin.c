extern int host_value(void);
int state = 35;
__attribute__((constructor)) static void init(void) { state += host_value(); }
int answer(void) { return state; }
