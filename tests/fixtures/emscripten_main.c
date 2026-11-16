#include <dlfcn.h>
#include <string.h>

int host_value(void) { return 7; }

int check_local(void) {
    void *h = dlopen("plugin.so", RTLD_NOW | RTLD_LOCAL);
    if (!h) return 1;
    int (*answer)(void) = dlsym(h, "answer");
    int *state = dlsym(h, "state");
    if (!answer || !state || answer() != 42 || *state != 42) return 2;
    *state = 43;
    if (answer() != 43) return 3;
    if (dlclose(h) || dlopen("plugin.so", RTLD_NOW) != h || answer() != 43) return 4;
    if (dlsym(RTLD_DEFAULT, "answer") != 0 || !dlerror() || dlerror()) return 5;
    return 0;
}
int check_global(void) {
    void *h = dlopen("global.so", RTLD_LAZY | RTLD_GLOBAL | RTLD_NODELETE);
    if (!h) return 1;
    int (*answer)(void) = dlsym(RTLD_DEFAULT, "answer");
    if (!answer || answer() != 42) return 2;
    if (dlopen("global.so", RTLD_NOW | RTLD_NOLOAD) != h) return 3;
    return 0;
}
int check_errors(void) {
    if (dlopen("missing%lib.so", RTLD_NOW)) return 1;
    char *error = dlerror();
    if (!error || !strstr(error, "missing%lib.so") || dlerror()) return 2;
    void *h = dlopen("plugin.so", RTLD_NOW);
    if (!h) return 3;
    if (dlsym(h, "missing%symbol") || !(error = dlerror()) || !strstr(error, "missing%symbol") || dlerror()) return 4;
    if (dlopen("never.so", RTLD_NOW | RTLD_NOLOAD) || !dlerror()) return 5;
    if (dlsym(RTLD_NEXT, "answer") || !dlerror()) return 6;
    if (dlsym((void *)123, "answer") || !dlerror()) return 7;
    return 0;
}
int check_main(void) {
    void *h = dlopen(0, RTLD_NOW);
    int (*value)(void) = dlsym(h, "host_value");
    return !h || !value || value() != 7;
}
