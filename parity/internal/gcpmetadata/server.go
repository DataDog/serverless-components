// Package gcpmetadata is a fake Google Cloud metadata server. Go serverless-init asks
// metadata.google.internal for the instance ID, the region and the project when it runs on Google
// Cloud; the parity harness gives that name to this server, so a scenario runs as if on Google
// Cloud Run without any Google Cloud account.
package gcpmetadata

import (
	"net/http"
	"time"
)

// Modes of the server. A scenario's metadata block selects one.
const (
	// ModeAnswer answers at once.
	ModeAnswer = "answer"
	// ModeSlow answers after Config.Delay.
	ModeSlow = "slow"
	// ModeFail returns status 500 after Config.Delay.
	ModeFail = "fail"
)

// Fixed test values. Real instance IDs are long lowercase strings; real regions are given as
// projects/<project number>/regions/<region>, and serverless-init keeps the last part.
const (
	InstanceID = "0000000000000000parityinstance01"
	Region     = "projects/123456789012/regions/us-central1"
	ProjectID  = "parity-project"
)

// Config sets how the server answers.
type Config struct {
	Mode  string
	Delay time.Duration
}

var values = map[string]string{
	"/computeMetadata/v1/instance/id":        InstanceID,
	"/computeMetadata/v1/instance/region":    Region,
	"/computeMetadata/v1/project/project-id": ProjectID,
}

// Handler answers the three paths that serverless-init reads, and returns 404 for any other path.
// Like the real server, it refuses a request without the header Metadata-Flavor: Google.
func Handler(config Config) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Metadata-Flavor", "Google")
		if r.Header.Get("Metadata-Flavor") != "Google" {
			http.Error(w, "missing the header Metadata-Flavor: Google", http.StatusForbidden)
			return
		}
		value, found := values[r.URL.Path]
		if !found {
			http.NotFound(w, r)
			return
		}
		if config.Mode == ModeSlow || config.Mode == ModeFail {
			select {
			case <-time.After(config.Delay):
			case <-r.Context().Done():
				return
			}
		}
		if config.Mode == ModeFail {
			http.Error(w, "metadata failure for the parity scenario", http.StatusInternalServerError)
			return
		}
		w.Header().Set("Content-Type", "application/text")
		_, _ = w.Write([]byte(value))
	})
}
