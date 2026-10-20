package main

import (
	"context"
	"flag"
	"log/slog"
	"os"
	"os/signal"
	"syscall"

	"llm-gateway/internal/config"
	"llm-gateway/internal/keymap"
	"llm-gateway/internal/server"
)

func main() {
	path := flag.String("config", "", "path to YAML configuration (default ~/.config/llm-gateway/config.yaml)")
	flag.Parse()
	logger := slog.New(slog.NewJSONHandler(os.Stdout, nil))
	configPath := *path
	if configPath == "" {
		defaultPath, err := config.DefaultPath()
		if err != nil {
			logger.Error("configuration failed", "error", err)
			os.Exit(1)
		}
		configPath = defaultPath
	}
	cfg, err := config.Load(configPath)
	if err != nil {
		logger.Error("configuration failed", "error", err)
		os.Exit(1)
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	srv := server.New(cfg.Listen, keymap.New(cfg.Keys), logger)
	logger.Info("starting", "listen", cfg.Listen)
	if err := server.Run(ctx, srv); err != nil {
		logger.Error("server failed", "error", err)
		os.Exit(1)
	}
}
