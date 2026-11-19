/* The one thing the recovery has to change about the phone's own wpa_supplicant:
 * it registers its AIDL interface with the service directory on starting, and
 * the recovery's directory cannot take it (it answers -129), which the
 * supplicant treats as fatal and ends on. Nothing here uses that interface:
 * the WLAN is driven over the control socket the supplicant opens beside it,
 * so the registration is answered as done and the program goes on. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <unistd.h>

static void say(const char *text, int status) {
    int fd = open("/dev/kmsg", O_WRONLY | O_CLOEXEC);
    if (fd < 0) return;
    char line[512];
    int n = snprintf(line, sizeof line,
                     "wpa: the directory would not take %s (%d); the WLAN is "
                     "driven over its control socket, going on\n", text, status);
    if (n > 0) {
        ssize_t w = write(fd, line, (size_t)n < sizeof line ? (size_t)n : sizeof line - 1);
        (void)w;
    }
    close(fd);
}

int32_t AServiceManager_addService(void *binder, const char *instance) {
    static int32_t (*real)(void *, const char *);
    static int said;
    if (!real) real = (int32_t (*)(void *, const char *))dlsym(RTLD_NEXT, "AServiceManager_addService");
    int32_t status = real ? real(binder, instance) : -1;
    if (status != 0 && !said) {
        said = 1;
        say(instance ? instance : "its interface", status);
    }
    return 0;
}
