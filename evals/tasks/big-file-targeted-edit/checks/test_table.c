#include <stdio.h>
int lookup_0207(int x); int lookup_0206(int x); int lookup_0208(int x); int lookup_0001(int x);
int main(void) {
    if (lookup_0207(3) != 228) { puts("0207 wrong"); return 1; }
    if (lookup_0206(3) != 3 * (206 % 13 + 1) + 206) { puts("0206 changed"); return 1; }
    if (lookup_0208(3) != 3 * (208 % 13 + 1) + 208) { puts("0208 changed"); return 1; }
    if (lookup_0001(3) != 3 * 2 + 1) { puts("0001 changed"); return 1; }
    puts("ok"); return 0;
}
