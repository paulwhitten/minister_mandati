#include <stdio.h>
int count_leq(const int *a, int n, int limit);
int main(void) {
    int a[] = {3, 5, 5, 8};
    if (count_leq(a, 4, 5) != 3) { puts("expected 3"); return 1; }
    if (count_leq(a, 4, 2) != 0) { puts("expected 0"); return 1; }
    if (count_leq(a, 4, 8) != 4) { puts("expected 4"); return 1; }
    puts("ok");
    return 0;
}
