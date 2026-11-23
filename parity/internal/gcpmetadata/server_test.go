package gcpmetadata

import (
	"io"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"
)

func get(t *testing.T, handler http.Handler, path string, flavor bool) (int, string) {
	t.Helper()
	request := httptest.NewRequest(http.MethodGet, path, nil)
	if flavor {
		request.Header.Set("Metadata-Flavor", "Google")
	}
	recorder := httptest.NewRecorder()
	handler.ServeHTTP(recorder, request)
	body, err := io.ReadAll(recorder.Result().Body)
	if err != nil {
		t.Fatal(err)
	}
	return recorder.Code, string(body)
}

func TestMetadataRequiresFlavorHeader(t *testing.T) {
	status, _ := get(t, Handler(Config{Mode: ModeAnswer}), "/computeMetadata/v1/instance/id", false)
	if status != http.StatusForbidden {
		t.Errorf("status = %d, want %d", status, http.StatusForbidden)
	}
}

func TestMetadataAnswersFixedValues(t *testing.T) {
	tests := []struct {
		mode       string
		path       string
		wantStatus int
		wantBody   string
	}{
		{ModeAnswer, "/computeMetadata/v1/instance/id", http.StatusOK, "0000000000000000parityinstance01"},
		{ModeAnswer, "/computeMetadata/v1/instance/region", http.StatusOK, "projects/123456789012/regions/us-central1"},
		{ModeAnswer, "/computeMetadata/v1/project/project-id", http.StatusOK, "parity-project"},
		{ModeAnswer, "/computeMetadata/v1/instance/zone", http.StatusNotFound, ""},
		{ModeFail, "/computeMetadata/v1/instance/id", http.StatusInternalServerError, ""},
	}
	for _, tt := range tests {
		t.Run(tt.mode+tt.path, func(t *testing.T) {
			status, body := get(t, Handler(Config{Mode: tt.mode}), tt.path, true)
			if status != tt.wantStatus {
				t.Errorf("status = %d, want %d", status, tt.wantStatus)
			}
			if tt.wantStatus == http.StatusOK && body != tt.wantBody {
				t.Errorf("body = %q, want %q", body, tt.wantBody)
			}
		})
	}
}

func TestMetadataSlowModeDelays(t *testing.T) {
	start := time.Now()
	status, body := get(t, Handler(Config{Mode: ModeSlow, Delay: 200 * time.Millisecond}), "/computeMetadata/v1/project/project-id", true)
	if elapsed := time.Since(start); elapsed < 200*time.Millisecond {
		t.Errorf("answered after %s, want at least 200ms", elapsed)
	}
	if status != http.StatusOK || body != "parity-project" {
		t.Errorf("got %d %q, want 200 \"parity-project\"", status, body)
	}
}
