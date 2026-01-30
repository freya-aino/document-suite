package src

import (
	"bytes"
	"io"
	"mime/multipart"
	"os"
	"reflect"
	"runtime"
)

func funcName(i interface{}) string {
	return runtime.FuncForPC(reflect.ValueOf(i).Pointer()).Name()
}

func GetFileData(file *multipart.FileHeader) ([]byte, error) {

	// open file
	fileSource, err := file.Open()
	if err != nil {
		return nil, err
	}
	defer fileSource.Close()

	fielData, err := io.ReadAll(fileSource)
	if err != nil {
		return nil, err
	}

	return fielData, nil
}

func WriteAsTemp(data *[]byte) (string, error) {
	tmp, err := os.CreateTemp("", "img-*")
	if err != nil {
		return "", err
	}

	reader := bytes.NewReader(*data)
	io.Copy(tmp, reader)
	tmp.Close()

	return tmp.Name(), nil
}
