// webcounter: the workload each Linux microVM runs.  Serves HTTPS on :443
// (self-signed, embedded certificate) and HTTP on :80, and answers every
// request with the VM id (from the conc_os hypercall), its hostname, and how
// many requests it has served since boot.  The counter lives in memory, so it
// survives freeze/thaw and is reset only by a real reboot or a fresh clone.
package main

import (
	"crypto/tls"
	_ "embed"
	"fmt"
	"log"
	"net/http"
	"os"
	"strings"
	"sync/atomic"
	"time"
)

//go:embed cert.pem
var certPEM []byte

//go:embed key.pem
var keyPEM []byte

const hcGetVmID = 7

var (
	hits    atomic.Int64
	started = time.Now()
)

func vmID() int64 {
	return int64(vmmcall(hcGetVmID))
}

func handler(w http.ResponseWriter, r *http.Request) {
	switch r.URL.Path {
	case "/reset":
		hits.Store(0)
		fmt.Fprintln(w, "reset")
		return
	case "/hits":
		fmt.Fprintln(w, hits.Load())
		return
	case "/sleep":
		// Simulates a slow backend: /sleep?ms=N
		ms := 100
		fmt.Sscanf(r.URL.Query().Get("ms"), "%d", &ms)
		time.Sleep(time.Duration(ms) * time.Millisecond)
	}
	n := hits.Add(1)
	host, _ := os.Hostname()
	proto := "http"
	if r.TLS != nil {
		proto = "https"
	}
	// One hypercall per request (each is a VM exit); the id cannot be cached
	// across requests because a clone keeps the parent's process state.
	id := vmID()
	w.Header().Set("Content-Type", "text/plain")
	w.Header().Set("X-Conc-VM", fmt.Sprint(id))
	fmt.Fprintf(w, "vm=%d host=%s hits=%d uptime=%.1fs proto=%s sni=%s path=%s\n",
		id, host, n, time.Since(started).Seconds(), proto, sni(r), r.URL.Path)
}

func sni(r *http.Request) string {
	if r.TLS != nil && r.TLS.ServerName != "" {
		return r.TLS.ServerName
	}
	if h := r.Host; h != "" {
		return strings.Split(h, ":")[0]
	}
	return "-"
}

func main() {
	log.SetFlags(0)
	cert, err := tls.X509KeyPair(certPEM, keyPEM)
	if err != nil {
		log.Fatalf("webcounter: bad embedded certificate: %v", err)
	}
	mux := http.NewServeMux()
	mux.HandleFunc("/", handler)
	go func() {
		log.Fatal(http.ListenAndServe(":80", mux))
	}()
	srv := &http.Server{
		Addr:    ":443",
		Handler: mux,
		TLSConfig: &tls.Config{
			Certificates: []tls.Certificate{cert},
			MinVersion:   tls.VersionTLS12,
		},
		ReadHeaderTimeout: 10 * time.Second,
	}
	fmt.Printf("webcounter: vm %d serving http :80 and https :443\n", vmID())
	log.Fatal(srv.ListenAndServeTLS("", ""))
}
