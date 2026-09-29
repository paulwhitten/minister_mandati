#include <math.h>
#include <stdio.h>

int main(int argc, char **argv) {
    (void)argv;
    double a = 3.0 + (argc - 1), b = 4.0;
    printf("hypot(3,4) = %.2f\n", hypot(a, b));
    return 0;
}
