package provider

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
)

// Anthropic speaks the Messages API.
type Anthropic struct {
	name     string
	endpoint string
	apiKey   string
	model    string
}

// Name implements Adapter.
func (a *Anthropic) Name() string { return a.name }

// Models implements Adapter.
func (a *Anthropic) Models() []string {
	if a.model != "" {
		return []string{a.model}
	}
	return []string{"claude-sonnet-4-20250514"}
}

type anRequest struct {
	Model     string      `json:"model"`
	MaxTokens int         `json:"max_tokens"`
	Messages  []anMessage `json:"messages"`
}

type anMessage struct {
	Role    string `json:"role"`
	Content string `json:"content"`
}

type anResponse struct {
	Content []struct {
		Type string `json:"type"`
		Text string `json:"text"`
	} `json:"content"`
	Usage struct {
		InputTokens  int `json:"input_tokens"`
		OutputTokens int `json:"output_tokens"`
	} `json:"usage"`
	Model string `json:"model"`
	Error *struct {
		Message string `json:"message"`
	} `json:"error,omitempty"`
}

// Execute implements Adapter.
func (a *Anthropic) Execute(ctx context.Context, req Request) (*Response, error) {
	model := req.Model
	if model == "" {
		model = a.model
	}
	maxOut := req.MaxOut
	if maxOut <= 0 {
		maxOut = 4096
	}
	body, err := json.Marshal(anRequest{
		Model:     model,
		MaxTokens: maxOut,
		Messages:  []anMessage{{Role: "user", Content: req.Prompt}},
	})
	if err != nil {
		return nil, err
	}
	httpReq, err := http.NewRequestWithContext(ctx, http.MethodPost, a.endpoint+"/messages", bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	httpReq.Header.Set("Content-Type", "application/json")
	httpReq.Header.Set("x-api-key", a.apiKey)
	httpReq.Header.Set("anthropic-version", "2023-06-01")
	resp, err := sharedClient.Do(httpReq)
	if err != nil {
		return nil, fmt.Errorf("%w: %v", ErrUnavailable, err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		io.Copy(io.Discard, io.LimitReader(resp.Body, 4096))
		return nil, classifyHTTP(resp.StatusCode)
	}
	var out anResponse
	if err := json.NewDecoder(resp.Body).Decode(&out); err != nil {
		return nil, fmt.Errorf("decode response: %w", err)
	}
	if out.Error != nil {
		return nil, fmt.Errorf("api error: %s", out.Error.Message)
	}
	text := ""
	for _, c := range out.Content {
		if c.Type == "text" {
			text += c.Text
		}
	}
	if text == "" {
		return nil, fmt.Errorf("empty response")
	}
	return &Response{
		Text:      text,
		TokensIn:  out.Usage.InputTokens,
		TokensOut: out.Usage.OutputTokens,
		Model:     out.Model,
	}, nil
}
