package main

import (
	"encoding/json"
	"net/http"
)

func (s *Server) handleQuota(w http.ResponseWriter, r *http.Request, u *Account) {
	w.Header().Set("Cache-Control", "no-store")
	var result json.RawMessage
	if err := s.agent.get(u.ID, "/internal/quota", &result); err != nil {
		writeAgentError(w, err)
		return
	}
	writeJSON(w, result)
}
