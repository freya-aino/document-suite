import torch
from transformers import AutoProcessor, Glm4vForConditionalGeneration
from os import environ

def main():
    
    processor = AutoProcessor.from_pretrained(environ["MODEL_PATH"])
    
    model = Glm4vForConditionalGeneration.from_pretrained(
        pretrained_model_name_or_path=environ["MODEL_PATH"],
        torch_dtype="auto",
        device_map="auto",
    )

    inputs = processor.apply_chat_template(
        "Hi how are you",
        tokenize=True,
        add_generation_prompt=True,
        return_dict=True,
        return_tensors="pt"
    ).to(model.device)

    inputs.pop("token_type_ids", None)
    generated_ids = model.generate(**inputs, max_new_tokens=8192)
    output_text = processor.decode(generated_ids[0][inputs["input_ids"].shape[1]:], skip_special_tokens=False)
    
    print(output_text)


if __name__ == "__main__":
    main()
