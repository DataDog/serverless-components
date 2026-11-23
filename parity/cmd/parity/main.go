// Command parity runs the parity harness: build builds the binaries, run compares a binary with
// the Go references, record updates the references, and stability checks that Go is stable.
package main

import (
	"flag"
	"fmt"
	"os"

	"github.com/DataDog/serverless-components/parity/internal/build"
)

const usage = `usage:
  parity build [flags] <go|rust|workload|fakeintake>
  parity run [--binary go|rust|<path>] [--scenario <name>]... [--parallel 4]
  parity record [--repin] [--scenario <name>]...
  parity stability [--runs 5] [--scenario <name>]...`

func main() {
	if len(os.Args) < 2 {
		fail(2, usage)
	}
	cfg, err := build.DefaultConfig()
	if err != nil {
		fail(1, "error: "+err.Error())
	}
	commands := map[string]func(build.Config, []string) error{
		"build": buildCommand, "run": runCommand, "record": recordCommand, "stability": stabilityCommand,
	}
	command, ok := commands[os.Args[1]]
	if !ok {
		fail(2, usage)
	}
	if err := command(cfg, os.Args[2:]); err != nil {
		fail(1, "error: "+err.Error())
	}
}

func fail(code int, message string) {
	fmt.Fprintln(os.Stderr, message)
	os.Exit(code)
}

func buildCommand(cfg build.Config, args []string) error {
	flags := flag.NewFlagSet("build", flag.ExitOnError)
	flags.StringVar(&cfg.AgentPath, "agent-path", cfg.AgentPath, "datadog-agent checkout")
	flags.StringVar(&cfg.CIDir, "ci-dir", cfg.CIDir, "serverless-init-ci checkout")
	flags.StringVar(&cfg.ComponentsDir, "components-dir", cfg.ComponentsDir, "serverless-components checkout")
	flags.StringVar(&cfg.Arch, "arch", cfg.Arch, "architecture to build for (arm64 or amd64)")
	flags.Parse(args)
	if flags.NArg() != 1 {
		fail(2, usage)
	}
	cfg, err := cfg.Resolve()
	if err != nil {
		return err
	}
	result, err := build.Build(flags.Arg(0), build.ExecRunner{}, cfg)
	if err != nil {
		return err
	}
	fmt.Println(result)
	return nil
}
