package main

import (
	"context"
	"flag"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"syscall"
	"time"

	"llm-gateway/internal/config"
	"llm-gateway/internal/daemon"
	"llm-gateway/internal/e2ee"
	"llm-gateway/internal/keymap"
	"llm-gateway/internal/proxy"
	"llm-gateway/internal/server"
)

// stopGrace exceeds the server's 10s drain window before --stop resorts to kill.
const stopGrace = 12 * time.Second

// version is stamped at build time via -ldflags "-X main.version=...".
var version = "dev"

func main() {
	path := flag.String("config", "", "path to YAML configuration (default ~/.config/llm-gateway/config.yaml)")
	background := flag.Bool("daemon", false, "run detached from the terminal; logs go to --log-file, use --stop to shut down")
	stopFlag := flag.Bool("stop", false, "stop the gateway recorded in the pid file")
	logPath := flag.String("log-file", "", "append logs to this file instead of stdout (default with --daemon: ~/.config/llm-gateway/llm-gateway.log)")
	pidPath := flag.String("pid-file", "", "path to the pid file (default ~/.config/llm-gateway/llm-gateway.pid)")
	versionFlag := flag.Bool("version", false, "print version and exit")
	keygenFlag := flag.Bool("keygen", false, "generate an e2ee identity keypair and exit")
	flag.Parse()
	if *keygenFlag {
		if err := e2ee.GenerateKeyPair(os.Stdout); err != nil {
			fmt.Fprintln(os.Stderr, "keygen failed:", err)
			os.Exit(1)
		}
		return
	}
	if *versionFlag {
		fmt.Println("llm-gateway", version)
		return
	}
	logger := slog.New(slog.NewJSONHandler(os.Stdout, nil))
	if *background && *stopFlag {
		logger.Error("configuration failed", "error", fmt.Errorf("--daemon and --stop cannot be combined"))
		os.Exit(1)
	}
	pidFile := *pidPath
	if pidFile == "" {
		resolved, err := defaultFile("llm-gateway.pid")
		if err != nil {
			logger.Error("configuration failed", "error", err)
			os.Exit(1)
		}
		pidFile = resolved
	}
	if *stopFlag {
		pid, err := daemon.Stop(pidFile, stopGrace)
		if err != nil {
			logger.Error("stop failed", "error", err, "pid_file", pidFile)
			os.Exit(1)
		}
		logger.Info("stopped", "pid", pid, "pid_file", pidFile)
		return
	}
	configPath := *path
	if configPath == "" {
		defaultPath, err := config.DefaultPath()
		if err != nil {
			logger.Error("configuration failed", "error", err)
			os.Exit(1)
		}
		configPath = defaultPath
	}
	if *background {
		if err := runBackground(logger, configPath, *logPath, pidFile); err != nil {
			logger.Error("background start failed", "error", err)
			os.Exit(1)
		}
		return
	}
	cfg, err := config.Load(configPath)
	if err != nil {
		logger.Error("configuration failed", "error", err)
		os.Exit(1)
	}
	if *logPath != "" {
		log, err := os.OpenFile(*logPath, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0600)
		if err != nil {
			logger.Error("configuration failed", "error", fmt.Errorf("open log file %s: %w", *logPath, err))
			os.Exit(1)
		}
		defer log.Close()
		os.Stdout, os.Stderr = log, log
		logger = slog.New(slog.NewJSONHandler(os.Stdout, nil))
	}
	if *pidPath != "" {
		held, err := daemon.RunningPID(*pidPath)
		if err != nil {
			logger.Error("configuration failed", "error", err)
			os.Exit(1)
		}
		if held != 0 {
			logger.Error("configuration failed", "error", fmt.Errorf("gateway is already running with pid %d (pid file %s)", held, *pidPath))
			os.Exit(1)
		}
		if err := daemon.WritePID(*pidPath, os.Getpid()); err != nil {
			logger.Error("configuration failed", "error", err)
			os.Exit(1)
		}
		defer daemon.RemovePID(*pidPath, os.Getpid())
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	keys := keymap.New(cfg.Keys)
	// e2ee: parse identity, build the peer dial table for the client-facing
	// proxy, and listen for encrypted connections from peer gateways.
	var peers map[string]proxy.PeerDialer
	var e2eeSrv *http.Server
	var e2eeLn net.Listener
	if cfg.E2EE != nil {
		identity, peersTable, allowed, err := buildE2EE(cfg, logger)
		if err != nil {
			logger.Error("configuration failed", "error", err)
			os.Exit(1)
		}
		peers = make(map[string]proxy.PeerDialer, len(peersTable))
		for addr, dialer := range peersTable {
			peers[addr] = dialer
		}
		e2eeLn, err = net.Listen("tcp", cfg.E2EE.Listen)
		if err != nil {
			logger.Error("e2ee listen failed", "error", err, "listen", cfg.E2EE.Listen)
			os.Exit(1)
		}
		wrapped := e2ee.NewListener(e2eeLn, identity, allowed, logger)
		e2eeSrv = server.NewWithPeers(cfg.E2EE.Listen, keys, logger, peers)
		logger.Info("e2ee listening", "listen", cfg.E2EE.Listen, "peers", len(cfg.E2EE.Peers))
		go func() {
			if err := server.Serve(ctx, e2eeSrv, wrapped); err != nil {
				logger.Error("e2ee server failed", "error", err)
				stop()
			}
		}()
	}
	srv := server.NewWithPeers(cfg.Listen, keys, logger, peers)
	logger.Info("starting", "listen", cfg.Listen, "version", version)
	if err := server.Run(ctx, srv); err != nil {
		logger.Error("server failed", "error", err)
		os.Exit(1)
	}
}

// buildE2EE parses the configured identity and builds one Noise dialer per
// peer, keyed by the peer's host:port so the proxy transport can match on the
// destination address alone. The returned allowlist maps peer names to their
// pinned public keys for the inbound listener.
func buildE2EE(cfg *config.Config, logger *slog.Logger) (*e2ee.Identity, map[string]*e2ee.Dialer, map[string][]byte, error) {
	identity, err := e2ee.ParseIdentity(cfg.E2EE.IdentityKey)
	if err != nil {
		return nil, nil, nil, fmt.Errorf("e2ee.identity_key: %w", err)
	}
	dialers := make(map[string]*e2ee.Dialer, len(cfg.E2EE.Peers))
	allowed := make(map[string][]byte, len(cfg.E2EE.Peers))
	for _, p := range cfg.E2EE.Peers {
		pub, err := e2ee.ParsePublicKey(p.IdentityPub)
		if err != nil {
			return nil, nil, nil, fmt.Errorf("e2ee peer %q identity_pub: %w", p.Name, err)
		}
		u, err := url.Parse(p.BaseURL) // Validated once at startup.
		if err != nil {
			return nil, nil, nil, fmt.Errorf("e2ee peer %q base_url: %w", p.Name, err)
		}
		dialers[u.Host] = e2ee.NewDialer(identity, map[string][]byte{u.Host: pub})
		allowed[p.Name] = pub
		logger.Info("e2ee peer configured", "peer", p.Name, "address", u.Host)
	}
	return identity, dialers, allowed, nil
}

// runBackground validates the config, relaunches the binary detached and waits
// until the child records its pid file and accepts connections.
func runBackground(logger *slog.Logger, configPath, logPath, pidFile string) error {
	cfg, err := config.Load(configPath)
	if err != nil {
		return err
	}
	held, err := daemon.RunningPID(pidFile)
	if err != nil {
		return err
	}
	if held != 0 {
		return fmt.Errorf("gateway is already running with pid %d (pid file %s)", held, pidFile)
	}
	if logPath == "" {
		if logPath, err = defaultFile("llm-gateway.log"); err != nil {
			return err
		}
	}
	exe, err := os.Executable()
	if err != nil {
		return fmt.Errorf("resolve executable: %w", err)
	}
	cmd, err := daemon.Start(exe, []string{"--config", configPath, "--log-file", logPath, "--pid-file", pidFile}, logPath)
	if err != nil {
		return err
	}
	if err := waitReady(cmd, cfg.Listen, pidFile, 5*time.Second); err != nil {
		return fmt.Errorf("%w (logs: %s)", err, logPath)
	}
	logger.Info("started in background", "pid", cmd.Process.Pid, "listen", cfg.Listen, "log", logPath, "pid_file", pidFile)
	return nil
}

// waitReady waits until the child has written the pid file and listens on the
// configured address, and reports a child that exits during startup.
func waitReady(cmd *exec.Cmd, listen, pidFile string, timeout time.Duration) error {
	exited := make(chan error, 1)
	go func() { exited <- cmd.Wait() }()
	deadline := time.Now().Add(timeout)
	for {
		select {
		case err := <-exited:
			return fmt.Errorf("process exited during startup: %v", err)
		default:
		}
		if pid, err := daemon.ReadPID(pidFile); err == nil && pid == cmd.Process.Pid {
			if conn, err := net.DialTimeout("tcp", probeAddress(listen), 250*time.Millisecond); err == nil {
				conn.Close()
				return nil
			}
		}
		if time.Now().After(deadline) {
			_ = cmd.Process.Kill()
			return fmt.Errorf("gateway did not start listening on %s within %s", listen, timeout)
		}
		time.Sleep(50 * time.Millisecond)
	}
}

// probeAddress turns a listen address like ":8080" into a dialable loopback one.
func probeAddress(listen string) string {
	host, port, err := net.SplitHostPort(listen)
	if err != nil {
		return listen
	}
	if host == "" || host == "0.0.0.0" || host == "::" {
		host = "127.0.0.1"
	}
	return net.JoinHostPort(host, port)
}

// defaultFile resolves a file inside the default configuration directory.
func defaultFile(name string) (string, error) {
	dir, err := config.DefaultDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(dir, name), nil
}
