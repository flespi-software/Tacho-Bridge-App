<template>
  <q-page class="row items-center justify-evenly">
    <tacho-main-component
      title="Tacho Main component"
      active
      :todos="todos"
      :meta="meta"
    ></tacho-main-component>
  </q-page>
</template>

<script setup lang="ts">
import { ref } from 'vue';
import { Todo, Meta } from 'components/models';
import TachoMainComponent from 'components/TachoMainComponent.vue';

defineOptions({
  name: 'IndexPage',
});

const todos = ref<Todo[]>([
  {
    id: 1,
    content: 'ct1',
  },
  {
    id: 2,
    content: 'ct2',
  },
  {
    id: 3,
    content: 'ct3',
  },
  {
    id: 4,
    content: 'ct4',
  },
  {
    id: 5,
    content: 'ct5',
  },
]);

const meta = ref<Meta>({
  totalCount: 1200,
});
</script>
