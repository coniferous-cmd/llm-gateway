package main

import (
	"llm-gateway/internal/config"
	"llm-gateway/internal/conversationlog"
	"log/slog"
)

func buildConversationRecorder(cfg *config.Config, logger *slog.Logger) *conversationlog.Store {
	if !cfg.ConversationLog.Enabled {
		return nil
	}
	return conversationlog.New(cfg.ConversationLog.Directory, cfg.ConversationLog.MaxBodyBytes, logger)
}
