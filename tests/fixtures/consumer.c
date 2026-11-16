extern int counter;
extern int add(int);
extern int (*exported_callback)(int);

int *counter_pointer = &counter;
int (*function_pointer)(int) = add;

__attribute__((constructor)) static void init(void) { counter += 3; }

int direct(void) { return add(32); }
int indirect(void) { return function_pointer(32); }
int relocated_data(void) { return *counter_pointer; }
int pointer_identity(void) { return function_pointer == exported_callback; }
int callback(int (*function)(int), int value) { return function(value); }
