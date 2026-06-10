package provider

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
)

// Gemini speaks the generateContent wire format.
type Gemini struct {
	name     string
	endpoint string
	apiKey   string
	model    string
}

// Name implements Adapter.
func (g *Gemini) Name() string { return g.name }

// Models implements Adapter.
func (g *Gemini) Models() []string {
	if g.model != "" {
		return []string{g.model}
	}
	return []string{"gemini-2.0-flash"}
}

type gmRequest struct {
	Contents         []gmContent         `json:"contents"`
	GenerationConfig *gmGenerationConfig `json:"generationConfig,omitempty"`
}

type gmContent struct {
	Role  string   `json:"role,omitempty"`
	Parts []gmPart `json:"parts"`
}

type gmPart struct {
	Text string `json:"text"`
}

type gmGenerationConfig struct {
	MaxOutputTokens int `json:"maxOutputTokens,omitempty"`
}

type gmResponse struct {
	Candidates []struct {
		Content struct {
			Parts []gmPart `json:"parts"`
		} `json:"content"`
	} `json:"candidates"`
	UsageMetadata struct {
		PromptTokenCount     int `json:"promptTokenCount"`
		CandidatesTokenCount int `json:"candidatesTokenCount"`
	} `json:"usageMetadata"`
	Error *struct {
		Message string `json:"message"`
	} `json:"error,omitempty"`
}

// Execute implements Adapter.
func (g *Gemini) Execute(ctx context.Context, req Request) (*Response, error) {
	model := req.Model
	if model == "" {
		model = g.model
	}
	var genCfg *gmGenerationConfig
	if req.MaxOut > 0 {
		genCfg = &gmGenerationConfig{MaxOutputTokens: req.MaxOut}
	}
	body, err := json.Marshal(gmRequest{
		Contents:         []gmContent{{Role: "user", Parts: []gmPart{{Text: req.Prompt}}}},
		GenerationConfig: genCfg,
	})
	if err != nil {
		return nil, err
	}
	url := fmt.Sprintf("%s/models/%s:generateContent?key=%s", g.endpoint, model, g.apiKey)
	httpReq, err := http.NewRequestWithContext(ctx, http.MethodPost, url, bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	httpReq.Header.Set("Content-Type", "application/json")
	resp, err := sharedClient.Do(httpReq)
	if err != nil {
		return nil, fmt.Errorf("%w: %v", ErrUnavailable, err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		io.Copy(io.Discard, io.LimitReader(resp.Body, 4096))
		return nil, classifyHTTP(resp.StatusCode)
	}
	var out gmResponse
	if err := json.NewDecoder(resp.Body).Decode(&out); err != nil {
		return nil, fmt.Errorf("decode response: %w", err)
	}
	if out.Error != nil {
		return nil, fmt.Errorf("api error: %s", out.Error.Message)
	}
	if len(out.Candidates) == 0 || len(out.Candidates[0].Content.Parts) == 0 {
		return nil, fmt.Errorf("empty response")
	}
	text := ""
	for _, p := range out.Candidates[0].Content.Parts {
		text += p.Text
	}
	return &Response{
		Text:      text,
		TokensIn:  out.UsageMetadata.PromptTokenCount,
		TokensOut: out.UsageMetadata.CandidatesTokenCount,
		Model:     model,
	}, nil
}
