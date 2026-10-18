#include <sys/resource.h>
#ifdef __APPLE__
#include <pthread/qos.h>
#endif
{
    /* Background work: lower CPU priority (and on macOS the utility QoS, a
     * lower IO tier). Threads started after this inherit it. */
#ifdef __APPLE__
    pthread_set_qos_class_self_np(QOS_CLASS_UTILITY, 0);
#endif
    setpriority(PRIO_PROCESS, 0, 10);
}
