package main

import (
	"context"
	"log"

	"github.com/modelcontextprotocol/go-sdk/mcp"
)

func main() {

	// TODO
	URL := "http://localhost:8080"

	ctx := context.Background()

	// Create client
	client := mcp.NewClient(&mcp.Implementation{Name: "mcp-client", Version: "v1.0.0"}, nil)

	// Connect to server
	session, err := client.Connect(
		ctx,
		&mcp.StreamableClientTransport{Endpoint: URL},
		nil,
	)
	if err != nil {
		log.Fatal(err)
	}
	defer session.Close()

	log.Println("Client session started")

	// Call a tool on the server.
	params := &mcp.CallToolParams{
		Name:      "greet",
		Arguments: map[string]any{"name": "you"},
	}
	res, err := session.CallTool(ctx, params)
	if err != nil {
		log.Fatalf("CallTool failed: %v", err)
	}
	if res.IsError {
		log.Fatal("tool failed")
	}
	for _, c := range res.Content {
		log.Print(c.(*mcp.TextContent).Text)
	}
}
