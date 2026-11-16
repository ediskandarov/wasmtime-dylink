/* Original, standalone fixture; no libc or application assets. */
int counter = 7;
int zeroes[8];

int add(int value) { return value + counter; }
int (*exported_callback)(int) = add;
int bss_is_zero(void) {
    int result = 0;
    for (int i = 0; i < 8; ++i) result |= zeroes[i];
    return result == 0;
}
