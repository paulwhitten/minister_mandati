#include "state.h"
#include "net.h"
int main(void) {
    load_config("/etc/demo.conf");
    persist_runtime_state("/run/demo");
    return serve_forever(8080);
}
