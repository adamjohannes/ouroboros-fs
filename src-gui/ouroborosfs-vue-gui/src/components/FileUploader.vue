<script setup lang="ts">
import { ref } from 'vue'
import { useNetworkStore } from '@/stores/network'

const store = useNetworkStore()
const fileInput = ref<HTMLInputElement | null>(null)
const selectedFile = ref<File | null>(null)

function triggerFileInput() {
  fileInput.value?.click()
}

function onFileSelected(event: Event) {
  const target = event.target as HTMLInputElement
  if (target.files && target.files.length > 0) {
    selectedFile.value = target.files[0]
    uploadFile()
  }
}

async function uploadFile() {
  if (!selectedFile.value) return

  const fileToUpload = selectedFile.value
  selectedFile.value = null

  await store.uploadFile(fileToUpload)
}
</script>

<template>
  <div class="file-uploader-container">
    <input
        type="file"
        ref="fileInput"
        @change="onFileSelected"
        style="display: none"
    />

    <button @click="triggerFileInput" :disabled="store.uploadLoading" class="upload-btn">
      <span v-if="store.uploadLoading">Uploading...</span>
      <span v-else>Share File with Network</span>
    </button>
  </div>
</template>

<style scoped>
.file-uploader-container {
  padding: 10px;
  border-top: 1px solid #eee;
  background-color: #fcfcfc;
}
.upload-btn {
  width: 100%;
  padding: 12px;
  font-size: 16px;
  font-weight: bold;
  color: #fff;
  background-color: #42b883;
  border: none;
  border-radius: 4px;
  cursor: pointer;
  transition: background-color 0.2s;
}
.upload-btn:hover {
  background-color: #369469;
}
.upload-btn:disabled {
  background-color: #aaa;
  cursor: not-allowed;
}
</style>