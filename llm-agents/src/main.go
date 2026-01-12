package main

import (
	"context"
	"encoding/json"
	"log"
	"net/http"
	"os"

	"github.com/tmc/langchaingo/llms"
	"github.com/tmc/langchaingo/llms/anthropic"
	"github.com/tmc/langchaingo/llms/openai"
)

type AvailabilityReturn struct {
	AnthropicAvailable bool `json:"anthropic_available"`
	OpenAIAvailable    bool `json:"openai_available"`
}

func checkOpenai() bool {
	_, openai_key_exists := os.LookupEnv("OPENAI_API_KEY")
	return openai_key_exists
}

func checkAnthropic() bool {
	_, anthropic_key_exists := os.LookupEnv("ANTHROPIC_API_KEY")
	return anthropic_key_exists
}

func check(w http.ResponseWriter, req *http.Request) {

	returnData := AvailabilityReturn{
		AnthropicAvailable: checkAnthropic(),
		OpenAIAvailable:    checkOpenai(),
	}

	w.Header().Set("Content-Type", "application/json")
	errResponse := json.NewEncoder(w).Encode(returnData)

	if errResponse != nil {
		w.WriteHeader(http.StatusBadRequest)
	}
}

func sayHelloOpenai(w http.ResponseWriter, req *http.Request) {

	ctx := context.Background()
	llm, err := openai.New()
	if err != nil {
		log.Fatal(err)
	}

	llm.GenerateContent(ctx, []llms.MessageContent{
		llms.TextParts(llms.ChatMessageTypeHuman, "What is the capital of France?"),
	})
}

func sayHelloAnthropic(w http.ResponseWriter, req *http.Request) {

	ctx := context.Background()
	llm, err := anthropic.New()
	if err != nil {
		log.Fatal(err)
	}

	llm.GenerateContent(ctx, []llms.MessageContent{
		llms.TextParts(llms.ChatMessageTypeHuman, "What is the capital of France?"),
	})

	// if err != nil {
	// 	fmt.Println(err)
	// } else {
	// 	fmt.Println(completion)
	// }
}

func main() {

	http.HandleFunc("/check", check)
	http.HandleFunc("/prompt/openai", sayHelloOpenai)
	http.HandleFunc("/prompt/anthropic", sayHelloAnthropic)

	http.ListenAndServe("localhost:8080", nil)

}
