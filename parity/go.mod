module github.com/DataDog/serverless-components/parity

go 1.26.0

require (
	github.com/DataDog/agent-payload/v5 v5.0.216
	github.com/DataDog/datadog-agent/pkg/proto v0.85.5
	github.com/DataDog/sketches-go v1.4.7
	github.com/benbjohnson/clock v1.3.5
	github.com/tinylib/msgp v1.6.5
	google.golang.org/protobuf v1.36.12
	gopkg.in/yaml.v3 v3.0.1
)

require (
	github.com/DataDog/datadog-agent/comp/core/tagger/origindetection v0.64.0 // indirect
	github.com/DataDog/datadog-agent/comp/netflow/payload v0.56.0-rc.3 // indirect
	github.com/DataDog/datadog-agent/pkg/metrics v0.64.0 // indirect
	github.com/DataDog/datadog-agent/pkg/network/payload v0.0.0-20250128160050-7ac9ccd58c07 // indirect
	github.com/DataDog/datadog-agent/pkg/networkpath/payload v0.0.0-20250128160050-7ac9ccd58c07 // indirect
	github.com/DataDog/datadog-agent/pkg/tagger/types v0.64.0 // indirect
	github.com/DataDog/mmh3 v0.0.0-20210722141835-012dc69a9e49 // indirect
	github.com/DataDog/zstd v1.5.8-0.20260421145859-31a7e515a571 // indirect
	github.com/beorn7/perks v1.0.1 // indirect
	github.com/cenkalti/backoff/v7 v7.0.3 // indirect
	github.com/cespare/xxhash/v2 v2.3.0 // indirect
	github.com/gogo/protobuf v1.3.2 // indirect
	github.com/google/uuid v1.6.0 // indirect
	github.com/klauspost/compress v1.20.4 // indirect
	github.com/munnerz/goautoneg v0.0.0-20191010083416-a7dc8b61c822 // indirect
	github.com/pandatix/go-abnf v0.5.5 // indirect
	github.com/planetscale/vtprotobuf v0.6.1-0.20240319094008-0393e58bdf10 // indirect
	github.com/prometheus/client_golang v1.25.0 // indirect
	github.com/prometheus/client_model v0.6.3 // indirect
	github.com/prometheus/common v0.74.0 // indirect
	github.com/prometheus/procfs v0.23.0 // indirect
	github.com/samber/lo v1.53.0 // indirect
	github.com/stretchr/testify v1.12.1 // indirect
	go.yaml.in/yaml/v3 v3.0.6 // indirect
	golang.org/x/net v0.62.0 // indirect
	golang.org/x/sys v0.50.0 // indirect
	golang.org/x/text v0.44.0 // indirect
	google.golang.org/genproto/googleapis/rpc v0.0.0-20261019180522-ce908d29299b // indirect
	google.golang.org/grpc v1.85.0 // indirect
)

require (
	github.com/DataDog/datadog-agent/test/fakeintake v0.0.0-00010101000000-000000000000
	github.com/philhofer/fwd v1.2.0 // indirect
)

replace github.com/DataDog/datadog-agent/test/fakeintake => ../../datadog-agent/test/fakeintake
