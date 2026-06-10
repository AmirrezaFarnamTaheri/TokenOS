package provider

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
)

// OpenAI speaks the /chat/completions wire format. It also serves any
// OpenAI-compatible endpoint (local proxies, Cursor/Windsurf bridges,
// llama.cpp, vLLM, OpenRouter ...).
type OpenAI struct {
	name     string
	endpoint string
	apiKey   string
	model    string
}

// Name implements Adapter.
func (o *OpenAI) Name() string { return o.name }

// Models implements Adapter (static manifest; live listing is an extra call).
func (o *OpenAI) Models() []string {
	if o.model != "" {
		return []string{o.model}
	}
	return []string{"gpt-4o-mini"}
}

type oaRequest struct {
	Model     string      `json:"model"`
	Messages  []oaMessage `json:"messages"`
	MaxTokens int         `json:"max_tokens,omitempty"`
}

type oaMessage struct {
	Role    string `json:"role"`
	Content string `json:"content"`
}

type oaResponse struct {
	Choices []struct {
		Message struct {
			Content string `json:"content"`
		} `json:"message"`
	} `json:"choices"`
	Usage struct {
		PromptTokens     int `json:"prompt_tokens"`
		CompletionTokens int `json:"completion_tokens"`
	} `json:"usage"`
	Model string `json:"model"`
	Error *struct {
		Message string `json:"message"`
	} `json:"error,omitempty"`
}

// Execute implements Adapter.
func (o *OpenAI) Execute(ctx context.Context, req Request) (*Response, error) {
	model := req.Model
	if model == "" {
		model = o.model
	}
	body, err := json.Marshal(oaRequest{
		Model:     model,
		Messages:  []oaMessage{{Role: "user", Content: req.Prompt}},
		MaxTokens: req.MaxOut,
	})
	if err != nil {
		return nil, err
	}
	httpReq, err := http.NewRequestWithContext(ctx, http.MethodPost, o.endpoint+"/chat/completions", bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	httpReq.Header.Set("Content-Type", "application/json")
	if o.apiKey != "" {
		httpReq.Header.Set("Authorization", "Bearer "+o.apiKey)
	}
	resp, err := sharedClient.Do(httpReq)
	if err != nil {
		return nil, fmt.Errorf("%w: %v", ErrUnavailable, err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		io.Copy(io.Discard, io.LimitReader(resp.Body, 4096))
		return nil, classifyHTTP(resp.StatusCode)
	}
	var out oaResponse
	if err := json.NewDecoder(resp.Body).Decode(&out); err != nil {
		return nil, fmt.Errorf("decode response: %w", err)
	}
	if out.Error != nil {
		return nil, fmt.Errorf("api error: %s", out.Error.Message)
	}
	if len(out.Choices) == 0 {
		return nil, fmt.Errorf("empty response")
	}
	return &Response{
		Text:      out.Choices[0].Message.Content,
		TokensIn:  out.Usage.PromptTokens,
		TokensOut: out.Usage.CompletionTokens,
		Model:     out.Model,
	}, nil
}
