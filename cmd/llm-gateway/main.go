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
	path := flag.String("config", "config.yaml", "path to YAML configuration")
	flag.Parse()
	logger := slog.New(slog.NewJSONHandler(os.Stdout, nil))
	cfg, err := config.Load(*path)
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
