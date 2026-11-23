// Command fake-gcp-metadata serves the fake Google Cloud metadata server of internal/gcpmetadata.
// The harness runs it in the Compose service fake-gcp-metadata, with the network alias
// metadata.google.internal.
package main

import (
	"flag"
	"fmt"
	"log"
	"net"
	"net/http"
	"os"
	"time"

	"github.com/DataDog/serverless-components/parity/internal/gcpmetadata"
)

func main() {
	address := flag.String("listen", ":80", "address to listen on")
	mode := flag.String("mode", gcpmetadata.ModeAnswer, "answer, slow or fail")
	delay := flag.Duration("delay", 0, "how long slow and fail modes wait before they respond")
	check := flag.Bool("check", false, "exit 0 if the server accepts connections on --listen, for the Compose health check")
	flag.Parse()
	if *check {
		_, port, err := net.SplitHostPort(*address)
		if err == nil {
			var connection net.Conn
			if connection, err = net.DialTimeout("tcp", net.JoinHostPort("127.0.0.1", port), time.Second); err == nil {
				connection.Close()
			}
		}
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		return
	}
	handler := gcpmetadata.Handler(gcpmetadata.Config{Mode: *mode, Delay: *delay})
	logged := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		log.Printf("%s %s Metadata-Flavor=%q", r.Method, r.URL.Path, r.Header.Get("Metadata-Flavor"))
		handler.ServeHTTP(w, r)
	})
	log.Printf("fake-gcp-metadata listening on %s, mode %s, delay %s", *address, *mode, *delay)
	log.Fatal(http.ListenAndServe(*address, logged))
}
