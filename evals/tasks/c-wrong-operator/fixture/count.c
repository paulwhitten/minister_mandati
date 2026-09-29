#include <stdio.h>

/* Number of values in a[0..n) that are <= limit. */
int count_leq(const int *a, int n, int limit) {
    int count = 0;
    for (int i = 0; i < n; i++) {
        if (a[i] < limit) {
            count++;
        }
    }
    return count;
}

#ifndef NO_MAIN
int main(void) {
    int a[] = {3, 5, 5, 8};
    printf("%d\n", count_leq(a, 4, 5));
    return 0;
}
#endif
