package src

import (
	"context"
	"fmt"
	"log"
	"os"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/credentials"
	"github.com/aws/aws-sdk-go-v2/service/s3"
	"go.temporal.io/sdk/client"
	"go.temporal.io/sdk/contrib/envconfig"

	"github.com/qdrant/go-client/qdrant"
)

func S3Client() *s3.Client {

	// TODO replace with environment read passed to docker container
	region := os.Getenv("AWS_REGION")
	endpoint := fmt.Sprintf("http://%s:%s", os.Getenv("RUSTFS_ADDRESS"), os.Getenv("RUSTFS_PORT"))
	accessKeyID := os.Getenv("RUSTFS_ACCESS_KEY")
	secretAccessKey := os.Getenv("RUSTFS_SECRET_KEY")

	if accessKeyID == "" {
		log.Fatalln("S3 accessKeyID env variable not set")
	}
	if secretAccessKey == "" {
		log.Fatalln("S3 secretAccessKey env variable not set")
	}
	if region == "" {
		log.Fatalln("S3 region env variable not set")
	}
	if endpoint == "" {
		log.Fatalln("S3 endpoint env variable not set")
	}

	// build aws.Config
	cfg := aws.Config{
		Region:      region,
		Credentials: aws.NewCredentialsCache(credentials.NewStaticCredentialsProvider(accessKeyID, secretAccessKey, "")),
	}

	client := s3.NewFromConfig(cfg, func(o *s3.Options) {
		o.UsePathStyle = true
		o.BaseEndpoint = aws.String(endpoint)
	})

	return client
}

func QdrantClient() *qdrant.Client {

	client, err := qdrant.NewClient(&qdrant.Config{
		Host: "qdrant",
		Port: 6334,
	})
	if err != nil {
		log.Fatalln("Unable to open Qdrant Client: ", err)
	}
	return client
}

func LoadTemporalConfigs(profile string) client.Options {

	// TODO get stage to insert into loading the config for devel, test, depl, etc.
	// STAGE := os.Getenv("STAGE")

	config, err := envconfig.LoadClientOptions(envconfig.LoadClientOptionsRequest{
		ConfigFilePath:    "./config.toml",
		ConfigFileProfile: profile,
	})
	if err != nil {
		log.Fatalln("Unable to find config file:", err)
	}
	return config
}

func StartWorkflow(workflow_options client.StartWorkflowOptions, workflow interface{}, args ...interface{}) {

	c, err := client.Dial(LoadTemporalConfigs("test"))
	if err != nil {
		log.Fatalln("Unable to create client", err)
	}
	defer c.Close()

	we, err := c.ExecuteWorkflow(
		context.Background(),
		workflow_options,
		workflow,
		args...,
	)

	if err != nil {
		log.Fatalln("Unable to execute Workflow", err)
	}
	log.Println("Started Workflow - WorkflowID:", we.GetID(), " - RunID:", we.GetRunID())

	var result any
	err = we.Get(context.Background(), &result)
	if err != nil {
		log.Fatalln("Unable to get workflow result", err)
	}
	log.Println("Workflow result:", result)
}
