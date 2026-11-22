/*
 * The SRT caller of the connect-order measurement (OL-64-0, #136; docs/specs/ios-app.md
 * section 14 Q13). Not part of any release: only the `connect-order` workflow builds and
 * runs it, against the Homebrew libsrt of a macOS runner.
 *
 *     ffmpeg ... -f mpegts pipe:1 | srt-sender <host> <port> [latency_ms] [inputbw_bytes]
 *
 * Reads MPEG-TS from stdin in 1316-byte payloads (7 TS packets) and sends each as one
 * SRT message, with the socket options the phone's sender is specified with (spec
 * section 11): SRTO_TRANSTYPE live, SRTO_LATENCY (= SRTO_PEERLATENCY) `latency_ms`
 * (default 200), SRTO_PAYLOADSIZE 1316, SRTO_MAXBW 0 with SRTO_INPUTBW `inputbw_bytes`
 * (bytes per second, the CBR rate) and SRTO_OHEADBW 25 %, no passphrase.
 *
 * Caller mode, dialling until a listener answers: while it is not connected the input is
 * read and discarded, so the encoder upstream stays real time and nothing is sent in a
 * burst once the link is up (the phone's sender drops while unconnected too). A link
 * that breaks is closed and dialled again; each connect is counted.
 *
 * Prints one JSON object per line on stdout, flushed: `{"event":"dialling"}` at start,
 * `{"event":"connected","connects":n}` and `{"event":"broken"}` as they happen, and
 * once a second while connected a `{"event":"stats",...}` line with libsrt's
 * cumulative counters for this connection (srt_bstats, the *Total fields) and the
 * send rate and RTT of the last second. Every line carries `t`, Unix time in ms.
 * Diagnostics go to stderr. Exits 0 at the end of stdin.
 */

#include <errno.h>
#include <inttypes.h>
#include <netdb.h>
#include <pthread.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

#include <srt/srt.h>

#define PAYLOAD 1316

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_mutex_t out_lock = PTHREAD_MUTEX_INITIALIZER;
static SRTSOCKET live = SRT_INVALID_SOCK; /* guarded by lock */
static volatile int send_failed = 0;      /* guarded by lock */
static volatile int input_ended = 0;

static long long now_ms(void) {
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return (long long)tv.tv_sec * 1000 + tv.tv_usec / 1000;
}

static void emit(const char *fmt, ...) __attribute__((format(printf, 1, 2)));

static void emit(const char *fmt, ...) {
    va_list ap;
    pthread_mutex_lock(&out_lock);
    printf("{\"t\":%lld,", now_ms());
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    printf("}\n");
    fflush(stdout);
    pthread_mutex_unlock(&out_lock);
}

/* Reads stdin for the whole run: sends each payload while a link is up, drops it
 * otherwise. */
static void *reader(void *unused) {
    (void)unused;
    char buf[PAYLOAD];
    for (;;) {
        size_t have = 0;
        while (have < PAYLOAD) {
            ssize_t n = read(STDIN_FILENO, buf + have, PAYLOAD - have);
            if (n == 0) {
                input_ended = 1;
                return NULL;
            }
            if (n < 0) {
                if (errno == EINTR) continue;
                fprintf(stderr, "srt-sender: reading stdin: %s\n", strerror(errno));
                input_ended = 1;
                return NULL;
            }
            have += (size_t)n;
        }
        pthread_mutex_lock(&lock);
        if (live != SRT_INVALID_SOCK && !send_failed) {
            if (srt_sendmsg2(live, buf, PAYLOAD, NULL) == SRT_ERROR) {
                fprintf(stderr, "srt-sender: send: %s\n", srt_getlasterror_str());
                send_failed = 1;
            }
        }
        pthread_mutex_unlock(&lock);
    }
}

static int set_int(SRTSOCKET s, SRT_SOCKOPT opt, int value, const char *name) {
    if (srt_setsockflag(s, opt, &value, sizeof value) == SRT_ERROR) {
        fprintf(stderr, "srt-sender: %s: %s\n", name, srt_getlasterror_str());
        return -1;
    }
    return 0;
}

static int set_i64(SRTSOCKET s, SRT_SOCKOPT opt, int64_t value, const char *name) {
    if (srt_setsockflag(s, opt, &value, sizeof value) == SRT_ERROR) {
        fprintf(stderr, "srt-sender: %s: %s\n", name, srt_getlasterror_str());
        return -1;
    }
    return 0;
}

static SRTSOCKET dial(const struct sockaddr *addr, socklen_t len, int latency, int64_t inputbw) {
    SRTSOCKET s = srt_create_socket();
    if (s == SRT_INVALID_SOCK) {
        fprintf(stderr, "srt-sender: create: %s\n", srt_getlasterror_str());
        return SRT_INVALID_SOCK;
    }
    int transtype = SRTT_LIVE;
    if (srt_setsockflag(s, SRTO_TRANSTYPE, &transtype, sizeof transtype) == SRT_ERROR
        || set_int(s, SRTO_LATENCY, latency, "SRTO_LATENCY")
        || set_int(s, SRTO_PAYLOADSIZE, PAYLOAD, "SRTO_PAYLOADSIZE")
        || set_i64(s, SRTO_MAXBW, 0, "SRTO_MAXBW")
        || set_i64(s, SRTO_INPUTBW, inputbw, "SRTO_INPUTBW")
        || set_int(s, SRTO_OHEADBW, 25, "SRTO_OHEADBW")
        || set_int(s, SRTO_CONNTIMEO, 2000, "SRTO_CONNTIMEO")) {
        srt_close(s);
        return SRT_INVALID_SOCK;
    }
    if (srt_connect(s, addr, (int)len) == SRT_ERROR) {
        srt_close(s);
        return SRT_INVALID_SOCK;
    }
    return s;
}

int main(int argc, char **argv) {
    if (argc < 3 || argc > 5) {
        fprintf(stderr, "usage: %s <host> <port> [latency_ms] [inputbw_bytes_per_s]\n", argv[0]);
        return 2;
    }
    const char *host = argv[1];
    const char *port = argv[2];
    int latency = argc > 3 ? atoi(argv[3]) : 200;
    int64_t inputbw = argc > 4 ? strtoll(argv[4], NULL, 10) : (4000000 + 128000) / 8;
    if (latency <= 0 || inputbw <= 0) {
        fprintf(stderr, "srt-sender: latency and inputbw must be positive\n");
        return 2;
    }

    struct addrinfo hints, *res = NULL;
    memset(&hints, 0, sizeof hints);
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_DGRAM;
    int gai = getaddrinfo(host, port, &hints, &res);
    if (gai != 0 || res == NULL) {
        fprintf(stderr, "srt-sender: cannot resolve the receiver: %s\n", gai_strerror(gai));
        return 1;
    }

    signal(SIGPIPE, SIG_IGN);
    if (srt_startup() < 0) {
        fprintf(stderr, "srt-sender: srt_startup failed\n");
        return 1;
    }
    srt_setloglevel(LOG_ERR);

    pthread_t thread;
    if (pthread_create(&thread, NULL, reader, NULL) != 0) {
        fprintf(stderr, "srt-sender: cannot start the reader\n");
        return 1;
    }

    emit("\"event\":\"dialling\",\"latency_ms\":%d,\"payload_size\":%d,\"inputbw\":%" PRId64
         ",\"oheadbw\":25", latency, PAYLOAD, inputbw);
    int connects = 0;
    while (!input_ended) {
        SRTSOCKET s = dial(res->ai_addr, res->ai_addrlen, latency, inputbw);
        if (s == SRT_INVALID_SOCK) {
            sleep(1);
            continue;
        }
        connects++;
        pthread_mutex_lock(&lock);
        live = s;
        send_failed = 0;
        pthread_mutex_unlock(&lock);
        emit("\"event\":\"connected\",\"connects\":%d", connects);

        for (;;) {
            sleep(1);
            if (input_ended) break;
            SRT_TRACEBSTATS p;
            memset(&p, 0, sizeof p);
            SRT_SOCKSTATUS state = srt_getsockstate(s);
            int failed;
            pthread_mutex_lock(&lock);
            failed = send_failed;
            pthread_mutex_unlock(&lock);
            if (srt_bstats(s, &p, 1) != SRT_ERROR) {
                emit("\"event\":\"stats\",\"connects\":%d,\"connected\":%s,"
                     "\"packets_sent\":%" PRId64 ",\"packets_sent_unique\":%" PRId64 ","
                     "\"packets_sent_dropped\":%d,\"packets_sent_lost\":%d,"
                     "\"packets_retransmitted\":%d,\"acks_received\":%d,\"naks_received\":%d,"
                     "\"bytes_sent\":%" PRIu64 ",\"bytes_sent_dropped\":%" PRIu64 ","
                     "\"rtt_ms\":%.3f,\"send_rate_mbps\":%.3f,\"bandwidth_mbps\":%.3f,"
                     "\"flight_size\":%d,\"send_buffer_ms\":%d",
                     connects, state == SRTS_CONNECTED ? "true" : "false",
                     p.pktSentTotal, p.pktSentUniqueTotal, p.pktSndDropTotal, p.pktSndLossTotal,
                     p.pktRetransTotal, p.pktRecvACKTotal, p.pktRecvNAKTotal, p.byteSentTotal,
                     p.byteSndDropTotal, p.msRTT, p.mbpsSendRate, p.mbpsBandwidth,
                     p.pktFlightSize, p.msSndBuf);
            }
            if (state != SRTS_CONNECTED || failed) {
                emit("\"event\":\"broken\",\"connects\":%d", connects);
                break;
            }
        }
        pthread_mutex_lock(&lock);
        live = SRT_INVALID_SOCK;
        pthread_mutex_unlock(&lock);
        srt_close(s);
    }
    emit("\"event\":\"input_ended\",\"connects\":%d", connects);
    pthread_join(thread, NULL);
    freeaddrinfo(res);
    srt_cleanup();
    return 0;
}
