package main

import (
	"errors"
	"fmt"
	"io"
	"log/slog"
	"strings"

	"github.com/spf13/cobra"

	"llm-gateway/internal/config"
	"llm-gateway/internal/daemon"
	"llm-gateway/internal/e2ee"
)

type options struct {
	configPath string
	logPath    string
	pidPath    string
	background bool
}

// cliError distinguishes usage errors (exit 2) and JSON-logged gateway errors
// from ordinary command failures (exit 1, written to stderr).
type cliError struct {
	err      error
	code     int
	logger   *slog.Logger
	reported bool
}

func (e *cliError) Error() string { return e.err.Error() }
func (e *cliError) Unwrap() error { return e.err }

func runCLI(args []string, stdout, stderr io.Writer) int {
	cmd := newRootCommand(stdout, stderr)
	cmd.SetArgs(normalizeLegacyFlags(args))
	if err := cmd.Execute(); err != nil {
		var failure *cliError
		if errors.As(err, &failure) {
			if failure.reported {
				return failure.code
			}
			if failure.logger != nil {
				failure.logger.Error("command failed", "error", err)
			} else {
				fmt.Fprintln(stderr, err)
			}
			return failure.code
		}
		fmt.Fprintln(stderr, err)
		return 1
	}
	return 0
}

// The original Go flag parser accepted -config as well as --config. Convert
// only known flags, leaving flag values and arguments after -- untouched.
func normalizeLegacyFlags(args []string) []string {
	args = append([]string(nil), args...)
	for i := 0; i < len(args); i++ {
		arg := args[i]
		if arg == "--" {
			break
		}
		if !strings.HasPrefix(arg, "-") {
			continue
		}
		name, _, hasValue := strings.Cut(strings.TrimPrefix(strings.TrimPrefix(arg, "-"), "-"), "=")
		switch name {
		case "config", "log-file", "pid-file", "daemon", "stop", "version", "keygen", "check-config":
			if !strings.HasPrefix(arg, "--") {
				args[i] = "-" + arg
			}
			if !hasValue && (name == "config" || name == "log-file" || name == "pid-file") {
				i++
			}
		}
	}
	return args
}

func newRootCommand(stdout, stderr io.Writer) *cobra.Command {
	var opts options
	var legacy struct{ stop, version, keygen, check bool }
	logger := slog.New(slog.NewJSONHandler(stdout, nil))
	gatewayError := func(err error) error {
		var failure *cliError
		if err == nil || errors.As(err, &failure) {
			return err
		}
		return &cliError{err: err, code: 1, logger: logger}
	}
	root := &cobra.Command{
		Use:           "llm-gateway",
		Short:         "An LLM API gateway; runs in the foreground by default",
		SilenceErrors: true,
		SilenceUsage:  true,
		Args: func(cmd *cobra.Command, args []string) error {
			if err := cobra.NoArgs(cmd, args); err != nil {
				return &cliError{err: err, code: 2}
			}
			return nil
		},
		RunE: func(cmd *cobra.Command, args []string) error {
			// Legacy flags retain their original precedence and ignored resources.
			switch {
			case legacy.check:
				for _, operation := range []struct {
					name    string
					enabled bool
				}{
					{"--daemon", opts.background}, {"--stop", legacy.stop},
					{"--keygen", legacy.keygen}, {"--version", legacy.version},
				} {
					if operation.enabled {
						return fmt.Errorf("configuration check failed: --check-config cannot be combined with %s", operation.name)
					}
				}
				return runCheckConfig(opts.configPath, stdout)
			case legacy.keygen:
				return generateKeys(stdout)
			case legacy.version:
				return printVersion(stdout)
			case legacy.stop && opts.background:
				return gatewayError(fmt.Errorf("--daemon and --stop cannot be combined"))
			case legacy.stop:
				return gatewayError(stopGateway(opts.pidPath, logger))
			default:
				return gatewayError(runGateway(opts, logger))
			}
		},
	}
	root.SetOut(stdout)
	root.SetErr(stderr)
	root.SetFlagErrorFunc(func(cmd *cobra.Command, err error) error {
		return &cliError{err: err, code: 2}
	})
	addServeFlags(root, &opts)
	// Local (not persistent) compatibility flags cannot leak into subcommands.
	root.Flags().BoolVar(&legacy.stop, "stop", false, "stop the gateway (prefer the stop command)")
	root.Flags().BoolVar(&legacy.version, "version", false, "print version (prefer the version command)")
	root.Flags().BoolVar(&legacy.keygen, "keygen", false, "generate an e2ee keypair (prefer the keygen command)")
	root.Flags().BoolVar(&legacy.check, "check-config", false, "validate configuration offline (prefer the check-config command)")

	var serveOpts options
	serve := &cobra.Command{
		Use: "serve", Short: "Run the gateway",
		Args: root.Args,
		RunE: func(cmd *cobra.Command, args []string) error {
			return gatewayError(runGateway(serveOpts, logger))
		},
	}
	addServeFlags(serve, &serveOpts)

	var pidPath string
	stop := &cobra.Command{
		Use: "stop", Short: "Stop the gateway recorded in the pid file",
		Args: root.Args,
		RunE: func(cmd *cobra.Command, args []string) error {
			return gatewayError(stopGateway(pidPath, logger))
		},
	}
	stop.Flags().StringVar(&pidPath, "pid-file", "", "path to the pid file (default ~/.config/llm-gateway/llm-gateway.pid)")

	var configPath string
	check := &cobra.Command{
		Use: "check-config", Short: "Validate configuration offline without startup side effects",
		Args: root.Args,
		RunE: func(cmd *cobra.Command, args []string) error {
			return runCheckConfig(configPath, stdout)
		},
	}
	addConfigFlag(check, &configPath)
	root.AddCommand(serve, stop, check,
		&cobra.Command{
			Use: "keygen", Short: "Generate an e2ee identity keypair",
			Args: root.Args,
			RunE: func(cmd *cobra.Command, args []string) error { return generateKeys(stdout) },
		},
		&cobra.Command{
			Use: "version", Short: "Print version",
			Args: root.Args,
			RunE: func(cmd *cobra.Command, args []string) error { return printVersion(stdout) },
		},
	)
	return root
}

func addConfigFlag(cmd *cobra.Command, path *string) {
	cmd.Flags().StringVar(path, "config", "", "path to YAML configuration (default ~/.config/llm-gateway/config.yaml)")
}

func addServeFlags(cmd *cobra.Command, opts *options) {
	addConfigFlag(cmd, &opts.configPath)
	cmd.Flags().BoolVar(&opts.background, "daemon", false, "run detached from the terminal")
	cmd.Flags().StringVar(&opts.logPath, "log-file", "", "append logs to this file (default with --daemon: ~/.config/llm-gateway/llm-gateway.log)")
	cmd.Flags().StringVar(&opts.pidPath, "pid-file", "", "path to the pid file (default ~/.config/llm-gateway/llm-gateway.pid)")
}

func printVersion(out io.Writer) error {
	_, err := fmt.Fprintln(out, "llm-gateway", version)
	return err
}

func generateKeys(out io.Writer) error {
	if err := e2ee.GenerateKeyPair(out); err != nil {
		return fmt.Errorf("keygen failed: %w", err)
	}
	return nil
}

func runCheckConfig(path string, out io.Writer) error {
	path, err := resolveConfigPath(path)
	if err == nil {
		_, err = config.Load(path)
	}
	if err != nil {
		return fmt.Errorf("configuration check failed: %w", err)
	}
	_, err = fmt.Fprintln(out, "configuration valid:", path)
	return err
}

func resolvePIDPath(path string) (string, error) {
	if path != "" {
		return path, nil
	}
	return defaultFile("llm-gateway.pid")
}

func stopGateway(path string, logger *slog.Logger) error {
	pidFile, err := resolvePIDPath(path)
	if err != nil {
		return fmt.Errorf("configuration failed: %w", err)
	}
	pid, err := daemon.Stop(pidFile, stopGrace)
	if err != nil {
		return fmt.Errorf("stop failed (pid file %s): %w", pidFile, err)
	}
	logger.Info("stopped", "pid", pid, "pid_file", pidFile)
	return nil
}

func runGateway(opts options, logger *slog.Logger) error {
	pidFile, err := resolvePIDPath(opts.pidPath)
	if err != nil {
		return fmt.Errorf("configuration failed: %w", err)
	}
	configPath, err := resolveConfigPath(opts.configPath)
	if err != nil {
		return fmt.Errorf("configuration failed: %w", err)
	}
	if opts.background {
		if err := runBackground(logger, configPath, opts.logPath, pidFile); err != nil {
			return fmt.Errorf("background start failed: %w", err)
		}
		return nil
	}
	return runForeground(opts, configPath, logger)
}
