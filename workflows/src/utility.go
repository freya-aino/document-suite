package src

import (
	"log"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/credentials"
	"github.com/aws/aws-sdk-go-v2/service/s3"
	"go.temporal.io/sdk/client"
	"go.temporal.io/sdk/contrib/envconfig"
)

func S3Client() *s3.Client {

	// TODO replace with environment read passed to docker container
	region := "eu-west-1"            // os.Getenv("AWS_REGION")
	endpoint := "rustfs:9001"        // os.Getenv("RUSTFS_ADDRESS"):os.Getenv("RUSTFS_PORT") INFO - REQUIRES INCLUDING THESE VARS IN DOCKER COMPOSE
	accessKeyID := "rustfsadmin"     // os.Getenv("RUSTFS_ACCESS_KEY")
	secretAccessKey := "rustfsadmin" // os.Getenv("RUSTFS_SECRET_KEY")

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
	})

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
